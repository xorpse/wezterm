use crate::pane::PaseoTerminalPane;
use async_trait::async_trait;
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use mux::pane::{alloc_pane_id, Pane};
use mux::tab::Tab;
use mux::window::WindowId;
use mux::Mux;
use parking_lot::Mutex;
use paseo_client::PaseoClient;
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use wezterm_term::TerminalSize;

#[derive(Clone)]
pub enum ConnectTarget {
    Relay {
        offer_url: String,
    },
    Local {
        host_port: String,
        use_tls: bool,
        password: Option<String>,
    },
}

pub struct PaseoDomain {
    domain_id: DomainId,
    name: String,
    target: ConnectTarget,
    client: Mutex<Option<PaseoClient>>,
    state: Mutex<DomainState>,
    attached_terminals: Mutex<HashSet<String>>,
    projects: Arc<Mutex<HashMap<String, String>>>,
}

impl PaseoDomain {
    pub fn new(name: impl Into<String>, target: ConnectTarget) -> Arc<PaseoDomain> {
        Arc::new(PaseoDomain {
            domain_id: alloc_domain_id(),
            name: name.into(),
            target,
            client: Mutex::new(None),
            state: Mutex::new(DomainState::Detached),
            attached_terminals: Mutex::new(HashSet::new()),
            projects: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn client_id(&self) -> String {
        format!("wezterm-paseo-{}", self.name)
    }

    pub fn client(&self) -> Option<PaseoClient> {
        self.client.lock().clone()
    }

    pub fn project_for_cwd(&self, cwd: &str) -> Option<String> {
        self.projects.lock().get(cwd).cloned()
    }

    pub async fn ensure_client(&self) -> anyhow::Result<PaseoClient> {
        if let Some(client) = self.client() {
            return Ok(client);
        }
        let client = self.connect().await?;
        *self.client.lock() = Some(client.clone());
        *self.state.lock() = DomainState::Attached;
        {
            let client = client.clone();
            promise::spawn::spawn(async move {
                let _ = client.run().await;
            })
            .detach();
        }
        {
            let client = client.clone();
            let projects = self.projects.clone();
            promise::spawn::spawn(async move {
                if let Ok(workspaces) = client.fetch_workspaces().await {
                    let mut map = projects.lock();
                    for ws in workspaces {
                        if !ws.project_display_name.is_empty() {
                            map.insert(ws.cwd().to_string(), ws.project_display_name.clone());
                        }
                    }
                }
            })
            .detach();
        }
        Ok(client)
    }

    async fn connect(&self) -> anyhow::Result<PaseoClient> {
        let client = match &self.target {
            ConnectTarget::Relay { offer_url } => {
                let offer = paseo_client::parse_offer_url(offer_url)?;
                PaseoClient::connect_relay(&offer, &self.client_id()).await?
            }
            ConnectTarget::Local {
                host_port,
                use_tls,
                password,
            } => {
                PaseoClient::connect_local(
                    host_port,
                    *use_tls,
                    password.as_deref(),
                    &self.client_id(),
                )
                .await?
            }
        };
        Ok(client)
    }
}

fn default_size() -> TerminalSize {
    TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

#[async_trait(?Send)]
impl Domain for PaseoDomain {
    async fn spawn_pane(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let client = self.ensure_client().await?;
        let cwd = command_dir.unwrap_or_else(|| "/tmp".to_string());
        let workspace_id = client.open_project(&cwd).await?;

        let (program, args) = match command {
            Some(builder) => {
                let argv: Vec<String> = builder
                    .get_argv()
                    .iter()
                    .map(|s| s.to_string_lossy().to_string())
                    .collect();
                match argv.split_first() {
                    Some((prog, rest)) => (Some(prog.clone()), Some(rest.to_vec())),
                    None => (None, None),
                }
            }
            None => (None, None),
        };

        let opts = paseo_client::CreateTerminalOpts {
            command: program,
            args,
            workspace_id: Some(workspace_id),
            rows: size.rows as u32,
            cols: size.cols as u32,
            ..Default::default()
        };
        let info = client.create_terminal(&cwd, opts).await?;
        let handle = client.subscribe_terminal(&info.id, "live").await?;
        let remote = handle.writer();
        let (pane, input_rx) = PaseoTerminalPane::new(
            alloc_pane_id(),
            self.domain_id,
            info.id.clone(),
            size,
            remote,
        );
        self.attached_terminals.lock().insert(info.id.clone());
        pane.start_io(handle, input_rx);
        Ok(pane as Arc<dyn Pane>)
    }

    fn spawnable(&self) -> bool {
        true
    }

    fn detachable(&self) -> bool {
        true
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        log::info!("paseo domain {} connecting", self.name);
        let client = self.ensure_client().await?;
        log::info!(
            "paseo domain {} connected to {:?} v{:?}",
            self.name,
            client.server_info().hostname,
            client.server_info().version
        );

        let mux = Mux::get();
        let window_id = match window_id {
            Some(window_id) => window_id,
            None => *mux.new_empty_window(None, None),
        };

        let size = default_size();
        let terminals = client.list_terminals(None).await?;
        log::info!(
            "paseo domain {} attaching {} terminals",
            self.name,
            terminals.len()
        );
        for info in terminals {
            if !self.attached_terminals.lock().insert(info.id.clone()) {
                continue;
            }
            let handle = client
                .subscribe_terminal(&info.id, "visible-snapshot")
                .await?;
            let remote = handle.writer();
            let (pane, input_rx) = PaseoTerminalPane::new(
                alloc_pane_id(),
                self.domain_id,
                info.id.clone(),
                size,
                remote,
            );

            let pane_dyn: Arc<dyn Pane> = pane.clone();
            mux.add_pane(&pane_dyn)?;

            let tab = Arc::new(Tab::new(&size));
            tab.assign_pane(&pane_dyn);
            mux.add_tab_and_active_pane(&tab)?;
            mux.add_tab_to_window(&tab, window_id)?;

            pane.start_io(handle, input_rx);
        }

        *self.state.lock() = DomainState::Attached;
        Ok(())
    }

    fn detach(&self) -> anyhow::Result<()> {
        *self.client.lock() = None;
        self.attached_terminals.lock().clear();
        *self.state.lock() = DomainState::Detached;
        Ok(())
    }

    fn state(&self) -> DomainState {
        *self.state.lock()
    }
}
