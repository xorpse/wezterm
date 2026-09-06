use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use mux::Mux;
use mux::domain::{DomainId, SplitSource};
use mux::pane::Pane;
use mux::tab::{SplitRequest, Tab};
use mux::window::WindowId;
use parking_lot::Mutex;
use serde_json::Value;
use wezterm_term::TerminalSize;

use crate::relay::{DEFAULT_WINDOW_SU, RelayConnection};
use crate::relay_pane::RelayPane;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct RelayBackend {
    domain_id: DomainId,
    name: String,
    target: String,
    connection: Mutex<Option<RelayConnection>>,
    panes: Mutex<HashMap<String, Weak<RelayPane>>>,
    owned: Mutex<HashSet<String>>,
    window: Mutex<Option<WindowId>>,
    detached: AtomicBool,
    supervising: AtomicBool,
}

impl RelayBackend {
    pub fn new(
        domain_id: DomainId,
        name: impl Into<String>,
        target: impl Into<String>,
    ) -> Arc<RelayBackend> {
        Arc::new(RelayBackend {
            domain_id,
            name: name.into(),
            target: target.into(),
            connection: Mutex::new(None),
            panes: Mutex::new(HashMap::new()),
            owned: Mutex::new(HashSet::new()),
            window: Mutex::new(None),
            detached: AtomicBool::new(false),
            supervising: AtomicBool::new(false),
        })
    }

    async fn ensure_connection(self: &Arc<Self>) -> anyhow::Result<RelayConnection> {
        if let Some(connection) = self.connection.lock().clone() {
            if !connection.is_closed() {
                return Ok(connection);
            }
        }
        let connection = RelayConnection::open(&self.target).await?;
        connection
            .open_client("subscriber", DEFAULT_WINDOW_SU)
            .await?;
        *self.connection.lock() = Some(connection.clone());
        self.detached.store(false, Ordering::SeqCst);
        self.start_poller();
        Ok(connection)
    }

    pub async fn attach(self: &Arc<Self>, window_id: Option<WindowId>) -> anyhow::Result<()> {
        log::info!(
            "orca host {} attaching via relay on {}",
            self.name,
            self.target
        );
        let connection = self.ensure_connection().await?;
        let mux = Mux::get();
        let window_id = match window_id {
            Some(window_id) => window_id,
            None => *mux.new_empty_window(None, None),
        };
        *self.window.lock() = Some(window_id);
        self.reconcile(&connection, window_id).await?;
        Ok(())
    }

    fn is_shown(&self, process: &Value) -> bool {
        let id = process
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        process.get("worktreeId").is_some()
            || process.get("terminalHandle").is_some()
            || self.owned.lock().contains(id)
    }

    async fn reconcile(
        self: &Arc<Self>,
        connection: &RelayConnection,
        window_id: WindowId,
    ) -> anyhow::Result<()> {
        let processes = connection.list_processes().await?;
        let mut live = processes
            .iter()
            .filter(|process| self.is_shown(process))
            .filter_map(|process| {
                process
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(|id| (id.to_owned(), process_cwd(process)))
            })
            .collect::<Vec<_>>();
        live.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let live_ids = live
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        let gone = self
            .panes
            .lock()
            .keys()
            .filter(|id| !live_ids.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for pty_id in gone {
            if let Some(weak) = self.panes.lock().remove(&pty_id) {
                if let Some(pane) = weak.upgrade() {
                    Mux::get().remove_pane(pane.pane_id());
                }
            }
            self.owned.lock().remove(&pty_id);
        }

        for (pty_id, cwd) in live {
            let present = self
                .panes
                .lock()
                .get(&pty_id)
                .is_some_and(|weak| weak.strong_count() > 0);
            if present {
                continue;
            }
            let owns = self.owned.lock().contains(&pty_id);
            match self
                .make_pane(
                    connection,
                    pty_id.clone(),
                    &cwd,
                    owns,
                    TerminalSize::default(),
                )
                .await
            {
                Ok(pane) => {
                    let pane_dyn = pane as Arc<dyn Pane>;
                    let tab = Arc::new(Tab::new(&TerminalSize::default()));
                    tab.assign_pane(&pane_dyn);
                    let mux = Mux::get();
                    mux.add_tab_and_active_pane(&tab)?;
                    mux.add_tab_to_window(&tab, window_id)?;
                }
                Err(err) => {
                    log::warn!(
                        "relay host {} could not attach {pty_id}: {err:#}",
                        self.name
                    );
                }
            }
        }
        Ok(())
    }

    async fn make_pane(
        &self,
        connection: &RelayConnection,
        pty_id: String,
        cwd: &str,
        owns_pty: bool,
        size: TerminalSize,
    ) -> anyhow::Result<Arc<RelayPane>> {
        let output = connection.route_pty(&pty_id);
        connection.attach_pty(&pty_id).await?;
        let pane_size = if owns_pty {
            connection
                .resize_pty(&pty_id, size.cols as u16, size.rows as u16)
                .await?;
            size
        } else {
            match connection.pty_size(&pty_id).await? {
                Some((cols, rows)) => TerminalSize {
                    cols: cols as usize,
                    rows: rows as usize,
                    ..size
                },
                None => size,
            }
        };
        let (pane, input_rx) = RelayPane::new(
            mux::pane::alloc_pane_id(),
            self.domain_id,
            pty_id.clone(),
            cwd.to_owned(),
            String::new(),
            owns_pty,
            pane_size,
            connection.clone(),
        );
        pane.start_io(output, input_rx);
        Mux::get().add_pane(&(pane.clone() as Arc<dyn Pane>))?;
        self.panes.lock().insert(pty_id, Arc::downgrade(&pane));
        Ok(pane)
    }

    pub async fn spawn_pane(
        self: &Arc<Self>,
        size: TerminalSize,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let connection = self.ensure_connection().await?;
        let pty_id = connection
            .spawn_pty(size.cols as u16, size.rows as u16, command_dir.as_deref())
            .await?;
        self.owned.lock().insert(pty_id.clone());
        let cwd = command_dir.unwrap_or_default();
        let pane = self
            .make_pane(&connection, pty_id, &cwd, true, size)
            .await?;
        Ok(pane as Arc<dyn Pane>)
    }

    pub async fn split_pane(
        self: &Arc<Self>,
        source: SplitSource,
        tab: mux::tab::TabId,
        pane_id: mux::pane::PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = Mux::get();
        let tab = mux
            .get_tab(tab)
            .ok_or_else(|| anyhow::anyhow!("invalid tab id {tab}"))?;
        let command_dir = match source {
            SplitSource::Spawn { command_dir, .. } => command_dir,
            SplitSource::MovePane(_) => {
                anyhow::bail!("moving panes into a relay host is not supported")
            }
        };
        let connection = self.ensure_connection().await?;
        let pane_index = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
            .map(|p| p.index)
            .ok_or_else(|| anyhow::anyhow!("invalid pane id {pane_id}"))?;
        let split_size = tab
            .compute_split_size(pane_index, split_request)
            .ok_or_else(|| anyhow::anyhow!("invalid pane index {pane_index}"))?;
        let cwd = command_dir.unwrap_or_default();
        let pty_id = connection
            .spawn_pty(
                split_size.second.cols as u16,
                split_size.second.rows as u16,
                if cwd.is_empty() { None } else { Some(&cwd) },
            )
            .await?;
        self.owned.lock().insert(pty_id.clone());
        let pane = self
            .make_pane(&connection, pty_id, &cwd, true, split_size.second)
            .await?;
        let pane_dyn = pane as Arc<dyn Pane>;
        tab.split_and_insert(pane_index, split_request, pane_dyn.clone())?;
        Ok(pane_dyn)
    }

    pub fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
        *self.connection.lock() = None;
        self.panes.lock().clear();
        self.owned.lock().clear();
        *self.window.lock() = None;
        let mux = Mux::get();
        let panes = mux
            .iter_panes()
            .into_iter()
            .filter(|pane| pane.domain_id() == self.domain_id)
            .map(|pane| pane.pane_id())
            .collect::<Vec<_>>();
        for pane_id in panes {
            mux.remove_pane(pane_id);
        }
    }

    fn start_poller(self: &Arc<Self>) {
        if self.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        // The timer runs on the smol executor (its reactor drives Timer); each
        // tick marshals the reconcile onto the main thread, where mux tab/pane
        // mutations must happen.
        promise::spawn::spawn(async move {
            loop {
                smol::Timer::after(POLL_INTERVAL).await;
                let Some(backend) = weak.upgrade() else {
                    return;
                };
                if backend.detached.load(Ordering::SeqCst) {
                    backend.supervising.store(false, Ordering::SeqCst);
                    return;
                }
                promise::spawn::spawn_into_main_thread(async move {
                    backend.poll_once().await;
                })
                .detach();
            }
        })
        .detach();
    }

    async fn poll_once(self: &Arc<Self>) {
        let Some(window_id) = *self.window.lock() else {
            return;
        };
        let connection = self.connection.lock().clone();
        let connection = match connection {
            Some(connection) if !connection.is_closed() => connection,
            _ => match self.reconnect().await {
                Some(connection) => connection,
                None => return,
            },
        };
        if let Err(err) = self.reconcile(&connection, window_id).await {
            log::warn!("relay host {} poll failed: {err:#}", self.name);
        }
    }

    async fn reconnect(self: &Arc<Self>) -> Option<RelayConnection> {
        let connection = match RelayConnection::open(&self.target).await {
            Ok(connection) => connection,
            Err(err) => {
                log::warn!("orca host {} relay reconnect failed: {err:#}", self.name);
                return None;
            }
        };
        if connection
            .open_client("subscriber", DEFAULT_WINDOW_SU)
            .await
            .is_err()
        {
            return None;
        }
        self.panes.lock().clear();
        let mux = Mux::get();
        let panes = mux
            .iter_panes()
            .into_iter()
            .filter(|pane| pane.domain_id() == self.domain_id)
            .map(|pane| pane.pane_id())
            .collect::<Vec<_>>();
        for pane_id in panes {
            mux.remove_pane(pane_id);
        }
        *self.connection.lock() = Some(connection.clone());
        log::info!(
            "orca host {} relay reconnected to {}",
            self.name,
            self.target
        );
        Some(connection)
    }
}

fn process_cwd(process: &Value) -> String {
    process
        .get("cwd")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_owned()
}
