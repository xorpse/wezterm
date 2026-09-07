use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use mux::Mux;
use mux::domain::{DomainId, SplitSource};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitDirection, SplitRequest, SplitSize, Tab};
use mux::window::WindowId;
use parking_lot::Mutex;
use serde_json::Value;
use wezterm_term::TerminalSize;

use crate::local_runtime::LocalRuntime;
use crate::relay::{DEFAULT_WINDOW_SU, RelayConnection};
use crate::relay_pane::RelayPane;

const POLL_INTERVAL: Duration = Duration::from_millis(1500);

struct TabState {
    signature: String,
    generation: u64,
    panes: Vec<Weak<RelayPane>>,
}

struct AppTab {
    parent_tab_id: String,
    worktree_key: String,
    worktree_path: String,
    layout: LayoutNode,
    pty_by_leaf: HashMap<String, String>,
}

enum LayoutNode {
    Leaf {
        leaf_id: String,
    },
    Split {
        direction: SplitDirection,
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    fn parse(value: &Value) -> Option<LayoutNode> {
        match value.get("type").and_then(|kind| kind.as_str())? {
            "leaf" => Some(LayoutNode::Leaf {
                leaf_id: value.get("leafId").and_then(|id| id.as_str())?.to_owned(),
            }),
            "split" => {
                // orca's 'vertical' divides along the width (panes side by side),
                // which is wezterm's Horizontal; 'horizontal' stacks them, which
                // is wezterm's Vertical.
                let direction = match value.get("direction").and_then(|d| d.as_str())? {
                    "vertical" => SplitDirection::Horizontal,
                    _ => SplitDirection::Vertical,
                };
                let ratio = value.get("ratio").and_then(|r| r.as_f64()).unwrap_or(0.5);
                let first = Box::new(LayoutNode::parse(value.get("first")?)?);
                let second = Box::new(LayoutNode::parse(value.get("second")?)?);
                Some(LayoutNode::Split {
                    direction,
                    ratio,
                    first,
                    second,
                })
            }
            _ => None,
        }
    }

    fn leftmost_leaf(&self) -> &str {
        match self {
            LayoutNode::Leaf { leaf_id } => leaf_id,
            LayoutNode::Split { first, .. } => first.leftmost_leaf(),
        }
    }

    fn pruned(&self, is_live: &dyn Fn(&str) -> bool) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf { leaf_id } => is_live(leaf_id).then(|| LayoutNode::Leaf {
                leaf_id: leaf_id.clone(),
            }),
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.pruned(is_live), second.pruned(is_live)) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(child), None) | (None, Some(child)) => Some(child),
                (None, None) => None,
            },
        }
    }

    /// A structural fingerprint (directions, tree shape and ptys, but not ratios)
    /// so the poller rebuilds a tab when the app's layout changes yet leaves it
    /// alone when only a split ratio is dragged.
    fn signature(&self, pty_by_leaf: &HashMap<String, String>) -> String {
        match self {
            LayoutNode::Leaf { leaf_id } => pty_by_leaf.get(leaf_id).cloned().unwrap_or_default(),
            LayoutNode::Split {
                direction,
                first,
                second,
                ..
            } => {
                let axis = match direction {
                    SplitDirection::Horizontal => 'h',
                    SplitDirection::Vertical => 'v',
                };
                format!(
                    "{axis}({},{})",
                    first.signature(pty_by_leaf),
                    second.signature(pty_by_leaf)
                )
            }
        }
    }
}

/// wezterm sizes the newly inserted (second) pane, so orca's first-child `ratio`
/// becomes the second child's percentage; clamp so neither pane collapses.
fn second_percent(ratio: f64) -> u8 {
    (((1.0 - ratio) * 100.0).round() as i64).clamp(1, 99) as u8
}

// Flipped as in LayoutNode::parse: a wezterm Horizontal (side-by-side) split is orca 'vertical'.
fn orca_direction(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Horizontal => "vertical",
        SplitDirection::Vertical => "horizontal",
    }
}

pub struct RuntimeBackend {
    domain_id: DomainId,
    name: String,
    target: String,
    target_id: String,
    runtime: LocalRuntime,
    relay: Mutex<Option<RelayConnection>>,
    relay_generation: AtomicU64,
    tabs: Mutex<HashMap<String, TabState>>,
    window: Mutex<Option<WindowId>>,
    active_worktree: Mutex<String>,
    worktrees: Mutex<HashMap<String, String>>,
    spawning: AtomicU64,
    detached: AtomicBool,
    supervising: AtomicBool,
}

struct SpawnGuard<'a>(&'a AtomicU64);

impl SpawnGuard<'_> {
    fn new(counter: &AtomicU64) -> SpawnGuard<'_> {
        counter.fetch_add(1, Ordering::SeqCst);
        SpawnGuard(counter)
    }
}

impl Drop for SpawnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RuntimeBackend {
    pub fn new(
        domain_id: DomainId,
        name: impl Into<String>,
        target: impl Into<String>,
        runtime: LocalRuntime,
        target_id: impl Into<String>,
    ) -> Arc<RuntimeBackend> {
        Arc::new(RuntimeBackend {
            domain_id,
            name: name.into(),
            target: target.into(),
            target_id: target_id.into(),
            runtime,
            relay: Mutex::new(None),
            relay_generation: AtomicU64::new(0),
            tabs: Mutex::new(HashMap::new()),
            window: Mutex::new(None),
            active_worktree: Mutex::new(String::new()),
            worktrees: Mutex::new(HashMap::new()),
            spawning: AtomicU64::new(0),
            detached: AtomicBool::new(false),
            supervising: AtomicBool::new(false),
        })
    }

    pub async fn discover(
        domain_id: DomainId,
        name: &str,
        target: &str,
    ) -> Option<Arc<RuntimeBackend>> {
        let runtime = LocalRuntime::discover()?;
        let targets = runtime.ssh_targets().await.ok()?;
        let target_id = targets
            .get("targets")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("label").and_then(|l| l.as_str()) == Some(target))?
            .get("id")?
            .as_str()?
            .to_owned();
        Some(Self::new(domain_id, name, target, runtime, target_id))
    }

    async fn ensure_relay(&self, want: &HashSet<String>) -> anyhow::Result<RelayConnection> {
        // The daemon can rotate out from under a live bridge (the app reconnects
        // and spawns a fresh daemon), so a connection that is not closed can still
        // no longer serve the session's ptys. Keep the cached one only while it
        // still carries at least one wanted pty.
        let cached = self.relay.lock().clone();
        if let Some(relay) = cached {
            if !relay.is_closed() && (want.is_empty() || Self::overlap(&relay, want).await > 0) {
                return Ok(relay);
            }
        }
        // A host can carry several relay daemons (from reconnects), each with its
        // own independent pty-N numbering, and the app's ptyId doesn't say which.
        // Pick the daemon that has the most of the app's session ptys live — that
        // is the one the runtime is actually driving.
        let daemons = crate::relay::discover_all_daemons(&self.target).await?;
        let mut best: Option<(usize, RelayConnection)> = None;
        for daemon in &daemons {
            let Ok(relay) = RelayConnection::open_daemon(&self.target, daemon).await else {
                continue;
            };
            if relay
                .open_client("subscriber", DEFAULT_WINDOW_SU)
                .await
                .is_err()
            {
                continue;
            }
            let overlap = Self::overlap(&relay, want).await;
            if best.as_ref().is_none_or(|(score, _)| overlap > *score) {
                best = Some((overlap, relay));
            }
        }
        let (overlap, relay) =
            best.ok_or_else(|| anyhow::anyhow!("no live relay daemon on {}", self.target))?;
        let generation = self.relay_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.relay.lock() = Some(relay.clone());
        log::info!(
            "orca host {} bound relay generation {generation} ({overlap}/{} wanted ptys live)",
            self.name,
            want.len()
        );
        Ok(relay)
    }

    async fn overlap(relay: &RelayConnection, want: &HashSet<String>) -> usize {
        if want.is_empty() {
            return 0;
        }
        relay
            .list_processes()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|process| process.get("id").and_then(|id| id.as_str()))
            .filter(|id| want.contains(*id))
            .count()
    }

    fn wanted_ptys(&self, session: &Value) -> HashSet<String> {
        self.parse_tabs(session)
            .into_iter()
            .flat_map(|tab| tab.pty_by_leaf.into_values())
            .collect()
    }

    fn refresh_worktrees(&self, session: &Value) {
        let Some(snapshots) = session.get("snapshots").and_then(|v| v.as_array()) else {
            return;
        };
        let mut map = self.worktrees.lock();
        for snapshot in snapshots {
            let Some(key) = snapshot.get("worktree").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some((_, path)) = key.split_once("::") else {
                continue;
            };
            map.insert(path.to_owned(), key.to_owned());
        }
    }

    pub fn local_runtime(&self) -> LocalRuntime {
        self.runtime.clone()
    }

    pub fn has_tab(&self, parent_tab_id: &str) -> bool {
        self.tabs.lock().contains_key(parent_tab_id)
    }

    pub fn close_tab(&self, parent_tab_id: &str) {
        self.remove_tab(parent_tab_id);
    }

    pub async fn open_tab(self: &Arc<Self>, parent_tab_id: &str, window: WindowId) -> bool {
        let session = match self.runtime.list_all().await {
            Ok(session) => session,
            Err(_) => return false,
        };
        let relay = match self.ensure_relay(&self.wanted_ptys(&session)).await {
            Ok(relay) => relay,
            Err(_) => return false,
        };
        let Some(tab) = self
            .parse_tabs(&session)
            .into_iter()
            .find(|tab| tab.parent_tab_id == parent_tab_id)
        else {
            return false;
        };
        let generation = self.relay_generation.load(Ordering::SeqCst);
        let signature = format!(
            "{}|{}",
            tab.worktree_path,
            tab.layout.signature(&tab.pty_by_leaf)
        );
        self.remove_tab(parent_tab_id);
        self.build_tab(&relay, window, &tab, signature, generation)
            .await;
        true
    }

    fn tab_window(&self, parent_tab_id: &str) -> Option<WindowId> {
        let panes = self
            .tabs
            .lock()
            .get(parent_tab_id)
            .map(|state| state.panes.clone())?;
        let mux = Mux::get();
        for weak in &panes {
            if let Some(pane) = weak.upgrade() {
                if let Some((_, window_id, _)) = mux.resolve_pane_id(pane.pane_id()) {
                    return Some(window_id);
                }
            }
        }
        None
    }

    fn worktree_key_for_cwd(&self, cwd: &str) -> Option<String> {
        self.worktrees
            .lock()
            .iter()
            .filter(|(path, _)| {
                cwd == path.as_str()
                    || cwd
                        .strip_prefix(path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(path, _)| path.len())
            .map(|(_, key)| key.clone())
    }

    pub fn project_for_cwd(&self, cwd: &str) -> Option<String> {
        let key = self.worktree_key_for_cwd(cwd)?;
        let path = key.split_once("::").map(|(_, path)| path).unwrap_or(&key);
        std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| Some(path.to_owned()))
    }

    pub async fn attach(self: &Arc<Self>, window_id: Option<WindowId>) -> anyhow::Result<()> {
        log::info!(
            "orca host {} attaching via local runtime session (target {})",
            self.name,
            self.target_id
        );
        let session = self.runtime.list_all().await?;
        let relay = self.ensure_relay(&self.wanted_ptys(&session)).await?;
        *self.window.lock() = window_id;
        self.apply(&relay, &session).await;
        self.start_poller();
        Ok(())
    }

    fn parse_tabs(&self, session: &Value) -> Vec<AppTab> {
        let prefix = format!("ssh:{}@@", self.target_id);
        let mut result = Vec::new();
        let Some(snapshots) = session.get("snapshots").and_then(|v| v.as_array()) else {
            return result;
        };
        for snapshot in snapshots {
            let worktree_key = snapshot
                .get("worktree")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let worktree_path = worktree_key
                .split_once("::")
                .map(|(_, path)| path.to_owned())
                .unwrap_or_else(|| worktree_key.clone());
            let Some(tabs) = snapshot.get("tabs").and_then(|v| v.as_array()) else {
                continue;
            };

            let mut seen = HashSet::new();
            for tab in tabs {
                let Some(pty_id) = tab.get("ptyId").and_then(|v| v.as_str()) else {
                    continue;
                };
                if !pty_id.starts_with(&prefix) {
                    continue;
                }
                let parent = tab
                    .get("parentTabId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                // Every leaf of a visual tab carries the same parentLayout, so the
                // first entry seen for a parentTabId describes the whole tree.
                if !seen.insert(parent.clone()) {
                    continue;
                }
                let Some((layout, pty_by_leaf)) = self.tab_layout(tab, &prefix) else {
                    continue;
                };
                result.push(AppTab {
                    parent_tab_id: parent,
                    worktree_key: worktree_key.clone(),
                    worktree_path: worktree_path.clone(),
                    layout,
                    pty_by_leaf,
                });
            }
        }
        result
    }

    fn tab_layout(
        &self,
        tab: &Value,
        prefix: &str,
    ) -> Option<(LayoutNode, HashMap<String, String>)> {
        let mut pty_by_leaf = HashMap::new();
        if let Some(map) = tab
            .pointer("/parentLayout/ptyIdsByLeafId")
            .and_then(|v| v.as_object())
        {
            for (leaf, pty) in map {
                let Some(pty) = pty.as_str() else { continue };
                if !pty.starts_with(prefix) {
                    continue;
                }
                let raw = pty.rsplit("@@").next().unwrap_or(pty).to_owned();
                pty_by_leaf.insert(leaf.clone(), raw);
            }
        }
        if !pty_by_leaf.is_empty() {
            if let Some(root) = tab
                .pointer("/parentLayout/root")
                .and_then(LayoutNode::parse)
            {
                return Some((root, pty_by_leaf));
            }
        }
        let leaf_id = tab
            .get("leafId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let pty = tab.get("ptyId").and_then(|v| v.as_str())?;
        let raw = pty.rsplit("@@").next().unwrap_or(pty).to_owned();
        let single = HashMap::from([(leaf_id.clone(), raw)]);
        Some((LayoutNode::Leaf { leaf_id }, single))
    }

    async fn apply(self: &Arc<Self>, relay: &RelayConnection, session: &Value) {
        let desired = self.parse_tabs(session);
        self.refresh_worktrees(session);
        if let Some(active) = desired.first() {
            *self.active_worktree.lock() = active.worktree_key.clone();
        }
        // Only reconcile tabs the user has actually opened or created; the hub
        // lists the rest and materialises them on demand. This keeps opened tabs
        // live (reconnect + split changes) without eagerly mirroring the session.
        let by_parent = desired
            .into_iter()
            .map(|tab| (tab.parent_tab_id.clone(), tab))
            .collect::<HashMap<_, _>>();
        let generation = self.relay_generation.load(Ordering::SeqCst);
        let materialised = self.tabs.lock().keys().cloned().collect::<Vec<_>>();
        for parent in materialised {
            let Some(tab) = by_parent.get(&parent) else {
                self.remove_tab(&parent);
                continue;
            };
            let signature = format!(
                "{}|{}",
                tab.worktree_path,
                tab.layout.signature(&tab.pty_by_leaf)
            );
            let previous = self
                .tabs
                .lock()
                .get(&parent)
                .map(|state| (state.signature.clone(), state.generation));
            if previous
                .as_ref()
                .is_some_and(|(sig, tracked)| *sig == signature && *tracked == generation)
            {
                continue;
            }
            log::info!(
                "orca host {} rebuilding {parent}: {previous:?} -> ({signature:?}, {generation})",
                self.name
            );
            let Some(window) = self.tab_window(&parent) else {
                continue;
            };
            self.remove_tab(&parent);
            self.build_tab(relay, window, tab, signature, generation)
                .await;
        }
    }

    fn remove_tab(&self, parent_tab_id: &str) {
        let Some(state) = self.tabs.lock().remove(parent_tab_id) else {
            return;
        };
        let mux = Mux::get();
        for weak in &state.panes {
            if let Some(pane) = weak.upgrade() {
                mux.remove_pane(pane.pane_id());
            }
        }
    }

    async fn build_tab(
        self: &Arc<Self>,
        relay: &RelayConnection,
        window_id: WindowId,
        tab: &AppTab,
        signature: String,
        generation: u64,
    ) {
        let size = TerminalSize::default();
        let mux = Mux::get();

        // The app persists tabs whose PTYs are dead but restorable, so a leaf can
        // fail to attach; prune those before realising the tree.
        let mut panes_by_leaf = HashMap::new();
        for (leaf_id, raw) in &tab.pty_by_leaf {
            if let Ok(pane) = self
                .make_pane(relay, raw, &tab.worktree_path, &tab.parent_tab_id, size)
                .await
            {
                panes_by_leaf.insert(leaf_id.clone(), pane);
            }
        }
        let Some(layout) = tab.layout.pruned(&|leaf| panes_by_leaf.contains_key(leaf)) else {
            return;
        };
        let Some(seed) = panes_by_leaf.get(layout.leftmost_leaf()).cloned() else {
            return;
        };

        let mux_tab = Arc::new(Tab::new(&size));
        mux_tab.assign_pane(&(seed.clone() as Arc<dyn Pane>));
        if mux.add_tab_and_active_pane(&mux_tab).is_err()
            || mux.add_tab_to_window(&mux_tab, window_id).is_err()
        {
            return;
        }

        let mut panes = vec![Arc::downgrade(&seed)];
        let mut stack = vec![(&layout, seed.pane_id())];
        while let Some((node, seed_id)) = stack.pop() {
            let LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } = node
            else {
                continue;
            };
            let Some(index) = Self::pane_index(&mux_tab, seed_id) else {
                continue;
            };
            let Some(second_pane) = panes_by_leaf.get(second.leftmost_leaf()).cloned() else {
                continue;
            };
            let request = SplitRequest {
                direction: *direction,
                target_is_second: true,
                top_level: false,
                size: SplitSize::Percent(second_percent(*ratio)),
            };
            if mux_tab
                .split_and_insert(index, request, second_pane.clone() as Arc<dyn Pane>)
                .is_ok()
            {
                panes.push(Arc::downgrade(&second_pane));
                stack.push((first.as_ref(), seed_id));
                stack.push((second.as_ref(), second_pane.pane_id()));
            }
        }

        log::info!(
            "orca host {} built tab {} with {} pane(s) (generation {generation})",
            self.name,
            tab.parent_tab_id,
            panes.len()
        );
        self.tabs.lock().insert(
            tab.parent_tab_id.clone(),
            TabState {
                signature,
                generation,
                panes,
            },
        );
    }

    fn pane_index(tab: &Tab, pane_id: PaneId) -> Option<usize> {
        tab.iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| positioned.pane.pane_id() == pane_id)
            .map(|positioned| positioned.index)
    }

    async fn make_pane(
        &self,
        relay: &RelayConnection,
        raw_pty: &str,
        cwd: &str,
        parent_tab_id: &str,
        size: TerminalSize,
    ) -> anyhow::Result<Arc<RelayPane>> {
        let output = relay.route_pty(raw_pty);
        if let Err(err) = relay.attach_pty(raw_pty).await {
            relay.unroute_pty(raw_pty);
            log::warn!(
                "orca host {} could not attach {raw_pty}: {err:#}",
                self.name
            );
            return Err(err);
        }
        let pane_size = match relay.pty_size(raw_pty).await? {
            Some((cols, rows)) => TerminalSize {
                cols: cols as usize,
                rows: rows as usize,
                ..size
            },
            None => size,
        };
        let (pane, input_rx) = RelayPane::new(
            mux::pane::alloc_pane_id(),
            self.domain_id,
            raw_pty.to_owned(),
            cwd.to_owned(),
            parent_tab_id.to_owned(),
            false,
            pane_size,
            relay.clone(),
        );
        pane.start_io(output, input_rx);
        Mux::get().add_pane(&(pane.clone() as Arc<dyn Pane>))?;
        Ok(pane)
    }

    pub async fn spawn_pane(
        self: &Arc<Self>,
        _size: TerminalSize,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let (pane, _parent) = self.create_and_track(command_dir).await?;
        Ok(pane)
    }

    async fn create_and_track(
        self: &Arc<Self>,
        command_dir: Option<String>,
    ) -> anyhow::Result<(Arc<dyn Pane>, String)> {
        let worktree = self.worktree_for(command_dir);
        self.create_pane(worktree, None).await
    }

    pub async fn spawn_agent(
        self: &Arc<Self>,
        cwd: &str,
        agent: &str,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let worktree = self.worktree_for(Some(cwd.to_owned()));
        let (pane, _parent) = self.create_pane(worktree, Some(agent.to_owned())).await?;
        Ok(pane)
    }

    async fn create_pane(
        self: &Arc<Self>,
        worktree: String,
        agent: Option<String>,
    ) -> anyhow::Result<(Arc<dyn Pane>, String)> {
        if worktree.is_empty() {
            anyhow::bail!("no active orca worktree yet; the session is still attaching");
        }
        // Hold off the poller from materialising this terminal while we create it,
        // otherwise both paths build a pane for the same pty and one is displaced.
        let _guard = SpawnGuard::new(&self.spawning);
        let created = match &agent {
            Some(agent) => self.runtime.create_agent_terminal(&worktree, agent).await?,
            None => self.runtime.create_terminal(&worktree).await?,
        };
        let tab_id = created
            .pointer("/tab/id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let parent = tab_id
            .split_once("::")
            .map(|(p, _)| p)
            .unwrap_or(tab_id)
            .to_owned();
        let pty_id = created
            .pointer("/tab/ptyId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("createTerminal returned no ptyId"))?;
        let raw = pty_id.rsplit("@@").next().unwrap_or(pty_id).to_owned();
        let cwd = worktree
            .split_once("::")
            .map(|(_, p)| p)
            .unwrap_or("")
            .to_owned();
        let relay = self.ensure_relay(&HashSet::new()).await?;
        let pane = self
            .make_pane(&relay, &raw, &cwd, &parent, TerminalSize::default())
            .await?;
        // Record it under its app parentTabId with the signature and generation the
        // poller would compute, so the poll treats it as already materialised.
        let signature = format!("{cwd}|{raw}");
        self.tabs.lock().insert(
            parent.clone(),
            TabState {
                signature,
                generation: self.relay_generation.load(Ordering::SeqCst),
                panes: vec![Arc::downgrade(&pane)],
            },
        );
        Ok((pane as Arc<dyn Pane>, parent))
    }

    pub async fn split_pane(
        self: &Arc<Self>,
        source: SplitSource,
        tab: mux::tab::TabId,
        pane_id: mux::pane::PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = Mux::get();
        let mux_tab = mux
            .get_tab(tab)
            .ok_or_else(|| anyhow::anyhow!("invalid tab id {tab}"))?;
        let command_dir = match source {
            SplitSource::Spawn { command_dir, .. } => command_dir,
            SplitSource::MovePane(_) => anyhow::bail!("moving panes is not supported"),
        };
        let panes = mux_tab.iter_panes_ignoring_zoom();
        let entry = panes
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
            .ok_or_else(|| anyhow::anyhow!("invalid pane id {pane_id}"))?;
        let source_pane = entry.pane.clone();
        let pane_index = entry.index;
        drop(panes);

        if let Some(pane) = self
            .split_via_runtime(&mux_tab, &source_pane, pane_index, split_request)
            .await?
        {
            return Ok(pane);
        }
        let (pane_dyn, _parent) = self.create_and_track(command_dir).await?;
        mux_tab.split_and_insert(pane_index, split_request, pane_dyn.clone())?;
        Ok(pane_dyn)
    }

    async fn split_via_runtime(
        self: &Arc<Self>,
        mux_tab: &Arc<Tab>,
        source_pane: &Arc<dyn Pane>,
        pane_index: usize,
        split_request: SplitRequest,
    ) -> anyhow::Result<Option<Arc<dyn Pane>>> {
        let (raw, parent) = {
            let Some(relay_pane) = source_pane.downcast_ref::<RelayPane>() else {
                return Ok(None);
            };
            (
                relay_pane.pty_id().to_owned(),
                relay_pane.parent_tab_id().to_owned(),
            )
        };

        let session = self.runtime.list_all().await?;
        let Some(handle) = Self::handle_for_raw(&session, &raw) else {
            return Ok(None);
        };
        let direction = orca_direction(split_request.direction);
        let worktree_path = self
            .parse_tabs(&session)
            .into_iter()
            .find(|tab| tab.parent_tab_id == parent)
            .map(|tab| tab.worktree_path)
            .unwrap_or_default();
        let before = self.tab_raw_ptys(&session, &parent);

        self.runtime.split_terminal(&handle, direction).await?;

        // The sibling leaf lands in the next inventory scan; retry in case it lags.
        let mut discovered = None;
        for _ in 0..5 {
            let session = self.runtime.list_all().await?;
            let after = self.tab_raw_ptys(&session, &parent);
            if let Some(new_raw) = after.difference(&before).next().cloned() {
                let signature = self.tab_signature(&session, &parent);
                discovered = Some((new_raw, signature));
                break;
            }
            smol::Timer::after(Duration::from_millis(150)).await;
        }
        let Some((new_raw, signature)) = discovered else {
            anyhow::bail!("orca accepted the split but the new pane never appeared");
        };

        let relay = self.ensure_relay(&HashSet::new()).await?;
        let new_pane = self
            .make_pane(
                &relay,
                &new_raw,
                &worktree_path,
                &parent,
                TerminalSize::default(),
            )
            .await?;
        mux_tab.split_and_insert(pane_index, split_request, new_pane.clone() as Arc<dyn Pane>)?;

        // Record the fresh two-leaf signature so the poller sees it as materialised.
        let generation = self.relay_generation.load(Ordering::SeqCst);
        let mut tabs = self.tabs.lock();
        let state = tabs.entry(parent).or_insert_with(|| TabState {
            signature: signature.clone().unwrap_or_default(),
            generation,
            panes: Vec::new(),
        });
        state.panes.push(Arc::downgrade(&new_pane));
        if let Some(signature) = signature {
            state.signature = signature;
        }
        state.generation = generation;

        Ok(Some(new_pane as Arc<dyn Pane>))
    }

    fn handle_for_raw(session: &Value, raw: &str) -> Option<String> {
        let snapshots = session.get("snapshots")?.as_array()?;
        for snapshot in snapshots {
            for tab in snapshot
                .get("tabs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if tab.get("type").and_then(|v| v.as_str()) != Some("terminal") {
                    continue;
                }
                let pty = tab.get("ptyId").and_then(|v| v.as_str()).unwrap_or("");
                if pty.rsplit("@@").next() != Some(raw) {
                    continue;
                }
                if let Some(handle) = tab.get("terminal").and_then(|v| v.as_str()) {
                    return Some(handle.to_owned());
                }
            }
        }
        None
    }

    fn tab_raw_ptys(&self, session: &Value, parent: &str) -> HashSet<String> {
        self.parse_tabs(session)
            .into_iter()
            .find(|tab| tab.parent_tab_id == parent)
            .map(|tab| tab.pty_by_leaf.into_values().collect())
            .unwrap_or_default()
    }

    fn tab_signature(&self, session: &Value, parent: &str) -> Option<String> {
        self.parse_tabs(session)
            .into_iter()
            .find(|tab| tab.parent_tab_id == parent)
            .map(|tab| {
                format!(
                    "{}|{}",
                    tab.worktree_path,
                    tab.layout.signature(&tab.pty_by_leaf)
                )
            })
    }

    fn worktree_for(&self, command_dir: Option<String>) -> String {
        if let Some(dir) = command_dir.as_deref() {
            if let Some(key) = self.worktree_key_for_cwd(dir) {
                return key;
            }
        }
        self.active_worktree.lock().clone()
    }

    pub fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
        *self.relay.lock() = None;
        self.tabs.lock().clear();
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
        if self.tabs.lock().is_empty() {
            return;
        }
        let session = match self.runtime.list_all().await {
            Ok(session) => session,
            Err(err) => {
                log::warn!("orca host {} list_all failed: {err:#}", self.name);
                return;
            }
        };
        let relay = match self.ensure_relay(&self.wanted_ptys(&session)).await {
            Ok(relay) => relay,
            Err(err) => {
                log::warn!("orca host {} has no live relay: {err:#}", self.name);
                return;
            }
        };
        self.apply(&relay, &session).await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LayoutNode, SplitDirection, orca_direction, second_percent};

    fn split_of(direction: &str, ratio: f64) -> LayoutNode {
        LayoutNode::parse(&json!({
            "type": "split",
            "direction": direction,
            "ratio": ratio,
            "first": { "type": "leaf", "leafId": "a" },
            "second": { "type": "leaf", "leafId": "b" },
        }))
        .expect("valid split")
    }

    #[test]
    fn orca_direction_maps_to_flipped_wezterm_axis() {
        let LayoutNode::Split { direction, .. } = split_of("vertical", 0.5) else {
            panic!("expected split");
        };
        assert_eq!(direction, SplitDirection::Horizontal);

        let LayoutNode::Split { direction, .. } = split_of("horizontal", 0.5) else {
            panic!("expected split");
        };
        assert_eq!(direction, SplitDirection::Vertical);
    }

    #[test]
    fn ratio_defaults_to_half_when_absent() {
        let node = LayoutNode::parse(&json!({
            "type": "split",
            "direction": "vertical",
            "first": { "type": "leaf", "leafId": "a" },
            "second": { "type": "leaf", "leafId": "b" },
        }))
        .expect("valid split");
        let LayoutNode::Split { ratio, .. } = node else {
            panic!("expected split");
        };
        assert_eq!(ratio, 0.5);
    }

    #[test]
    fn orca_direction_inverts_the_layout_axis() {
        assert_eq!(orca_direction(SplitDirection::Horizontal), "vertical");
        assert_eq!(orca_direction(SplitDirection::Vertical), "horizontal");
    }

    #[test]
    fn second_percent_sizes_the_new_pane_from_first_ratio() {
        assert_eq!(second_percent(0.5), 50);
        assert_eq!(second_percent(0.7), 30);
        assert_eq!(second_percent(0.25), 75);
        assert_eq!(second_percent(1.0), 1);
        assert_eq!(second_percent(0.0), 99);
    }

    #[test]
    fn leftmost_leaf_descends_first_children() {
        let node = LayoutNode::parse(&json!({
            "type": "split",
            "direction": "vertical",
            "ratio": 0.5,
            "first": {
                "type": "split",
                "direction": "horizontal",
                "ratio": 0.5,
                "first": { "type": "leaf", "leafId": "deep" },
                "second": { "type": "leaf", "leafId": "b" },
            },
            "second": { "type": "leaf", "leafId": "c" },
        }))
        .expect("valid split");
        assert_eq!(node.leftmost_leaf(), "deep");
    }

    #[test]
    fn signature_tracks_structure_not_ratio() {
        let ptys = [
            ("a".to_owned(), "pty-1".to_owned()),
            ("b".to_owned(), "pty-2".to_owned()),
        ]
        .into_iter()
        .collect();
        let wide = split_of("vertical", 0.7).signature(&ptys);
        let narrow = split_of("vertical", 0.3).signature(&ptys);
        assert_eq!(wide, narrow);

        let stacked = split_of("horizontal", 0.7).signature(&ptys);
        assert_ne!(wide, stacked);
    }

    #[test]
    fn prune_collapses_splits_that_lose_a_child() {
        let node = split_of("vertical", 0.5);
        let only_b = node.pruned(&|leaf| leaf == "b").expect("survives");
        assert!(matches!(only_b, LayoutNode::Leaf { leaf_id } if leaf_id == "b"));

        assert!(node.pruned(&|_| false).is_none());
    }
}
