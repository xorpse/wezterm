use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex as AsyncMutex;
use mux::domain::{Domain, DomainId, DomainState, SplitSource, alloc_domain_id};
use mux::pane::{Pane, alloc_pane_id};
use mux::tab::{SplitDirection, SplitRequest, SplitSize as MuxSplitSize, Tab, TabId};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use orca_client::{
    CreateTerminalOpts, LayoutNode, OrcaClient, PairingOffer, ServerDir, SessionTabsEvent,
    SplitDirection as OrcaSplitDirection, SshConnectionState, SshTargetSummary, TerminalSummary,
    VisualLayout, VisualPaneNode, VisualTab, WorktreePsSummary, id_selector,
};
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use wezterm_term::TerminalSize;

use crate::pane::{OrcaTerminalPane, TerminalBinding};

const RECONNECT_MIN_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const PUBLISH_DEBOUNCE: Duration = Duration::from_millis(500);

struct ApplyGuard(Arc<AtomicU64>);

impl ApplyGuard {
    fn hold(counter: &Arc<AtomicU64>) -> ApplyGuard {
        counter.fetch_add(1, Ordering::SeqCst);
        ApplyGuard(counter.clone())
    }
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct SplitOp {
    split_from: String,
    insert: String,
    direction: SplitDirection,
    first_handles: Vec<String>,
    second_handles: Vec<String>,
}

struct RealiseContext<'a> {
    summaries: &'a HashMap<String, TerminalSummary>,
    ratios: &'a HashMap<String, f64>,
    live: &'a HashMap<String, mux::pane::PaneId>,
}

fn split_percent(ratio: Option<f64>) -> u8 {
    let first = ratio.unwrap_or(0.5).clamp(0.01, 0.99);
    (((1.0 - first) * 100.0).round() as u8).clamp(1, 99)
}

fn leftmost_layout_leaf(node: &LayoutNode) -> Option<&str> {
    let mut node = node;
    loop {
        match node {
            LayoutNode::Leaf { leaf_id } => {
                return (!leaf_id.is_empty()).then_some(leaf_id.as_str());
            }
            LayoutNode::Split { first, .. } => node = first,
        }
    }
}

fn collect_layout_ratios(root: &LayoutNode, out: &mut HashMap<String, f64>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } = node
        {
            if let Some(leaf) = leftmost_layout_leaf(second) {
                out.insert(leaf.to_owned(), ratio.unwrap_or(0.5));
            }
            stack.push(second);
            stack.push(first);
        }
    }
}

fn layout_leaf_ids(root: &LayoutNode) -> Vec<String> {
    let mut leaves = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            LayoutNode::Leaf { leaf_id } => leaves.push(leaf_id.clone()),
            LayoutNode::Split { first, second, .. } => {
                stack.push(second);
                stack.push(first);
            }
        }
    }
    leaves
}

#[derive(Clone, Copy)]
struct PaneRect {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}

struct PublishTarget {
    worktree: String,
    parent_tab_id: String,
    leaf_panes: HashMap<String, mux::pane::PaneId>,
    rects: HashMap<mux::pane::PaneId, PaneRect>,
    pane_order: Vec<mux::pane::PaneId>,
    split_directions: Vec<SplitDirection>,
}

fn layout_matches_target(root: &LayoutNode, target: &PublishTarget) -> bool {
    let mut leaves = Vec::new();
    let mut directions = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            LayoutNode::Leaf { leaf_id } => leaves.push(leaf_id.as_str()),
            LayoutNode::Split {
                direction,
                first,
                second,
                ..
            } => {
                directions.push(visual_direction(direction));
                stack.push(second);
                stack.push(first);
            }
        }
    }
    leaves.len() == target.pane_order.len()
        && leaves
            .iter()
            .zip(&target.pane_order)
            .all(|(leaf, pane_id)| target.leaf_panes.get(*leaf).copied() == Some(*pane_id))
        && directions == target.split_directions
}

fn measure_layout_ratios(root: &mut LayoutNode, target: &PublishTarget) -> bool {
    let mut changed = false;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } = node
        {
            let horizontal = direction != "vertical";
            let extent = |node: &LayoutNode| -> Option<usize> {
                let mut lo = usize::MAX;
                let mut hi = 0usize;
                for leaf in layout_leaf_ids(node) {
                    let pane_id = target.leaf_panes.get(&leaf)?;
                    let rect = target.rects.get(pane_id)?;
                    let (start, len) = if horizontal {
                        (rect.left, rect.width)
                    } else {
                        (rect.top, rect.height)
                    };
                    lo = lo.min(start);
                    hi = hi.max(start + len);
                }
                (lo < hi).then_some(hi - lo)
            };
            if let (Some(first_extent), Some(second_extent)) = (extent(first), extent(second)) {
                let measured = first_extent as f64 / (first_extent + second_extent) as f64;
                let rounded = (measured * 1000.0).round() / 1000.0;
                if ratio.is_none_or(|current| (current - rounded).abs() > 0.01) {
                    *ratio = Some(rounded);
                    changed = true;
                }
            }
            stack.push(second);
            stack.push(first);
        }
    }
    changed
}

async fn publish_tab_layouts(
    domain_id: DomainId,
    dirty: Arc<Mutex<HashSet<TabId>>>,
    order: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    loop {
        smol::Timer::after(PUBLISH_DEBOUNCE).await;
        let batch = dirty.lock().drain().collect::<Vec<_>>();
        let order_dirty = order.swap(false, Ordering::SeqCst);
        if batch.is_empty() && !order_dirty {
            running.store(false, Ordering::SeqCst);
            if (dirty.lock().is_empty() && !order.load(Ordering::SeqCst))
                || running.swap(true, Ordering::SeqCst)
            {
                return;
            }
            continue;
        }
        let Some(domain) = Mux::get().get_domain(domain_id) else {
            running.store(false, Ordering::SeqCst);
            return;
        };
        let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
            running.store(false, Ordering::SeqCst);
            return;
        };
        for tab_id in batch {
            if let Err(err) = orca.publish_tab_layout(tab_id).await {
                log::debug!("orca: layout publish for tab {tab_id} failed: {err:#}");
            }
        }
        if order_dirty {
            if let Err(err) = orca.publish_tab_order().await {
                log::debug!("orca: tab order publish failed: {err:#}");
            }
        }
    }
}

fn majority_tab(
    members: &[(String, mux::pane::PaneId, mux::tab::TabId)],
) -> Option<mux::tab::TabId> {
    let mut counts = HashMap::new();
    for (_, _, tab_id) in members {
        *counts.entry(*tab_id).or_insert(0usize) += 1;
    }
    let best = counts.values().copied().max()?;
    members
        .iter()
        .map(|(_, _, tab_id)| *tab_id)
        .find(|tab_id| counts[tab_id] == best)
}

fn collect_agent_statuses(
    event: &SessionTabsEvent,
    statuses: &mut HashMap<String, Option<String>>,
) {
    for tab in &event.tabs {
        if tab.kind != "terminal" {
            continue;
        }
        let Some(terminal) = &tab.terminal else {
            continue;
        };
        statuses.insert(
            terminal.clone(),
            tab.agent_status.as_ref().map(|status| status.state.clone()),
        );
    }
}

fn free_local_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

async fn wait_for_local_port(port: u16) -> anyhow::Result<()> {
    for _ in 0..25 {
        if smol::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        smol::Timer::after(Duration::from_millis(200)).await;
    }
    anyhow::bail!("ssh tunnel on 127.0.0.1:{port} did not come up")
}

fn detach_live_pane(pane_id: mux::pane::PaneId) -> Option<Arc<dyn Pane>> {
    let mux = Mux::get();
    let (_, _, tab_id) = mux.resolve_pane_id(pane_id)?;
    let tab = mux.get_tab(tab_id)?;
    let pane = tab.remove_pane(pane_id)?;
    if tab.is_dead() {
        mux.remove_tab(tab_id);
    }
    Some(pane)
}

fn visual_direction(direction: &str) -> SplitDirection {
    match direction {
        "vertical" => SplitDirection::Vertical,
        _ => SplitDirection::Horizontal,
    }
}

fn leftmost_terminal(node: &VisualPaneNode) -> &str {
    let mut node = node;
    loop {
        match node {
            VisualPaneNode::Terminal { handle, .. } => return handle,
            VisualPaneNode::PaneSplit { first, .. } => node = first,
        }
    }
}

fn pane_tree_handles(root: &VisualPaneNode) -> Vec<String> {
    let mut handles = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            VisualPaneNode::Terminal { handle, .. } => handles.push(handle.clone()),
            VisualPaneNode::PaneSplit { first, second, .. } => {
                stack.push(second);
                stack.push(first);
            }
        }
    }
    handles
}

fn plan_splits(root: &VisualPaneNode) -> Vec<SplitOp> {
    let mut ops = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let VisualPaneNode::PaneSplit {
            direction,
            first,
            second,
        } = node
        {
            ops.push(SplitOp {
                split_from: leftmost_terminal(first).to_owned(),
                insert: leftmost_terminal(second).to_owned(),
                direction: visual_direction(direction),
                first_handles: pane_tree_handles(first),
                second_handles: pane_tree_handles(second),
            });
            stack.push(second);
            stack.push(first);
        }
    }
    ops
}

#[derive(Clone)]
struct WorktreeEntry {
    id: String,
    display_name: String,
    path: String,
}

#[derive(Clone)]
pub enum RuntimeTarget {
    Direct(PairingOffer),
    Ssh { target: String },
}

#[derive(Clone, Default)]
pub struct HubTerminal {
    pub handle: String,
    pub worktree_id: String,
    pub worktree_path: String,
    pub parent_tab_id: String,
    pub leaf_id: String,
    pub title: String,
    pub agent: Option<String>,
    pub connected: bool,
}

pub struct OrcaDomain {
    domain_id: DomainId,
    name: String,
    target: RuntimeTarget,
    tunnel: Mutex<Option<std::process::Child>>,
    client: Arc<Mutex<Option<OrcaClient>>>,
    state: Arc<Mutex<DomainState>>,
    connection: Arc<AtomicU64>,
    attached_terminals: Mutex<HashSet<String>>,
    attach_window: Mutex<Option<WindowId>>,
    topology: AsyncMutex<()>,
    worktrees: Arc<Mutex<HashMap<String, WorktreeEntry>>>,
    applying: Arc<AtomicU64>,
    publish_dirty: Arc<Mutex<HashSet<TabId>>>,
    publish_order: Arc<AtomicBool>,
    publish_running: Arc<AtomicBool>,
    publish_subscribed: AtomicBool,
    relay: Arc<crate::relay_backend::RelayBackend>,
    relay_mode: AtomicBool,
    runtime: Mutex<Option<Arc<crate::runtime_backend::RuntimeBackend>>>,
    runtime_mode: AtomicBool,
    ssh_attach_claimed: AtomicBool,
}

impl OrcaDomain {
    pub fn new(name: impl Into<String>, offer: PairingOffer) -> OrcaDomain {
        Self::with_target(name, RuntimeTarget::Direct(offer))
    }

    pub fn new_ssh(name: impl Into<String>, target: impl Into<String>) -> OrcaDomain {
        Self::with_target(
            name,
            RuntimeTarget::Ssh {
                target: target.into(),
            },
        )
    }

    fn with_target(name: impl Into<String>, target: RuntimeTarget) -> OrcaDomain {
        let domain_id = alloc_domain_id();
        let name = name.into();
        let relay_target = match &target {
            RuntimeTarget::Ssh { target } => target.clone(),
            RuntimeTarget::Direct(_) => String::new(),
        };
        let relay = crate::relay_backend::RelayBackend::new(domain_id, name.clone(), relay_target);
        OrcaDomain {
            domain_id,
            name,
            target,
            tunnel: Mutex::new(None),
            client: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(DomainState::Detached)),
            connection: Arc::new(AtomicU64::new(0)),
            attached_terminals: Mutex::new(HashSet::new()),
            attach_window: Mutex::new(None),
            topology: AsyncMutex::new(()),
            worktrees: Arc::new(Mutex::new(HashMap::new())),
            applying: Arc::new(AtomicU64::new(0)),
            publish_dirty: Arc::new(Mutex::new(HashSet::new())),
            publish_order: Arc::new(AtomicBool::new(false)),
            publish_running: Arc::new(AtomicBool::new(false)),
            publish_subscribed: AtomicBool::new(false),
            relay,
            relay_mode: AtomicBool::new(false),
            runtime: Mutex::new(None),
            runtime_mode: AtomicBool::new(false),
            ssh_attach_claimed: AtomicBool::new(false),
        }
    }

    async fn resolve_ssh_offer(
        &self,
        target: &str,
        refresh_pairing: bool,
    ) -> anyhow::Result<PairingOffer> {
        let runtime = crate::ssh::ensure_remote_runtime(target, refresh_pairing).await?;
        let offer = PairingOffer::parse(&runtime.pairing_url)?;
        let local_port = free_local_port()?;
        self.spawn_tunnel(target, local_port, runtime.remote_port)?;
        wait_for_local_port(local_port).await?;
        Ok(offer.with_endpoint(format!("ws://127.0.0.1:{local_port}")))
    }

    fn spawn_tunnel(&self, target: &str, local_port: u16, remote_port: u16) -> anyhow::Result<()> {
        self.stop_tunnel();
        let child = std::process::Command::new("ssh")
            .arg("-N")
            .arg("-oBatchMode=yes")
            .arg("-oConnectTimeout=10")
            .arg("-oExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        *self.tunnel.lock() = Some(child);
        Ok(())
    }

    fn stop_tunnel(&self) {
        if let Some(mut child) = self.tunnel.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn client(&self) -> Option<OrcaClient> {
        self.client.lock().clone()
    }

    pub fn reset_client(&self) {
        self.connection.fetch_add(1, Ordering::SeqCst);
        *self.client.lock() = None;
        *self.state.lock() = DomainState::Detached;
    }

    pub fn project_for_cwd(&self, cwd: &str) -> Option<String> {
        if self.runtime_mode.load(Ordering::SeqCst) {
            let runtime = self.runtime.lock().clone();
            if let Some(runtime) = runtime {
                return runtime.project_for_cwd(cwd);
            }
        }
        self.worktree_for_cwd(cwd)
            .map(|entry| entry.display_name.clone())
    }

    pub fn is_runtime_mode(&self) -> bool {
        self.runtime_mode.load(Ordering::SeqCst)
    }

    fn runtime_backend(&self) -> Option<Arc<crate::runtime_backend::RuntimeBackend>> {
        if self.runtime_mode.load(Ordering::SeqCst) {
            self.runtime.lock().clone()
        } else {
            None
        }
    }

    pub async fn hub_worktree_ps(&self) -> anyhow::Result<Vec<WorktreePsSummary>> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().worktree_ps().await;
        }
        Ok(self.ensure_client().await?.worktree_ps().await?)
    }

    pub async fn hub_list_terminals(&self) -> anyhow::Result<Vec<HubTerminal>> {
        if let Some(runtime) = self.runtime_backend() {
            let session = runtime.local_runtime().list_all().await?;
            return Ok(Self::hub_terminals_from_session(&session));
        }
        let terminals = self.ensure_client().await?.list_terminals(None).await?;
        Ok(terminals
            .into_iter()
            .filter(|terminal| terminal.pty_id.is_some())
            .map(|terminal| {
                let title = match &terminal.title {
                    Some(title) if !title.is_empty() => title.clone(),
                    _ => terminal
                        .preview
                        .lines()
                        .next()
                        .unwrap_or(&terminal.handle)
                        .trim()
                        .to_owned(),
                };
                HubTerminal {
                    handle: terminal.handle,
                    worktree_id: terminal.worktree_id,
                    worktree_path: terminal.worktree_path,
                    parent_tab_id: terminal.tab_id,
                    leaf_id: terminal.leaf_id,
                    title,
                    agent: terminal.agent_identity,
                    connected: terminal.connected,
                }
            })
            .collect())
    }

    fn hub_terminals_from_session(session: &serde_json::Value) -> Vec<HubTerminal> {
        let mut terminals = Vec::new();
        // The app session can carry stale tabs that point at a PTY already owned by
        // another tab; a PTY backs exactly one terminal, so keep the first and drop
        // the rest rather than materialising two panes that fight over it.
        let mut seen_ptys = HashSet::new();
        let Some(snapshots) = session.get("snapshots").and_then(|v| v.as_array()) else {
            return terminals;
        };
        for snapshot in snapshots {
            let key = snapshot
                .get("worktree")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let (worktree_id, worktree_path) = key
                .split_once("::")
                .map(|(id, path)| (id.to_owned(), path.to_owned()))
                .unwrap_or_else(|| (String::new(), key.to_owned()));
            let Some(tabs) = snapshot.get("tabs").and_then(|v| v.as_array()) else {
                continue;
            };
            for tab in tabs {
                let Some(pty_id) = tab.get("ptyId").and_then(|v| v.as_str()) else {
                    continue;
                };
                if !seen_ptys.insert(pty_id.to_owned()) {
                    continue;
                }
                let parent_tab_id = tab
                    .get("parentTabId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let leaf_id = tab
                    .get("leafId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let title = tab
                    .get("title")
                    .and_then(|v| v.as_str())
                    .filter(|title| !title.is_empty())
                    .unwrap_or("terminal")
                    .to_owned();
                terminals.push(HubTerminal {
                    handle: leaf_id.clone(),
                    worktree_id: worktree_id.clone(),
                    worktree_path: worktree_path.clone(),
                    parent_tab_id,
                    leaf_id,
                    title,
                    agent: None,
                    connected: true,
                });
            }
        }
        terminals
    }

    pub async fn hub_ssh_targets(&self) -> anyhow::Result<Vec<SshTargetSummary>> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().ssh_target_summaries().await;
        }
        Ok(self.ensure_client().await?.list_ssh_targets().await?)
    }

    pub async fn hub_ssh_target_state(
        &self,
        target_id: &str,
    ) -> anyhow::Result<Option<SshConnectionState>> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().ssh_target_state(target_id).await;
        }
        Ok(self
            .ensure_client()
            .await?
            .ssh_target_state(target_id)
            .await?)
    }

    pub async fn hub_detect_agents(&self) -> anyhow::Result<Vec<String>> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().detect_agents().await;
        }
        Ok(self
            .ensure_client()
            .await?
            .detect_agents()
            .await
            .unwrap_or_default())
    }

    pub async fn hub_spawn_agent(
        &self,
        worktree_selector: &str,
        worktree_path: &str,
        agent: &str,
        size: TerminalSize,
        window_id: WindowId,
    ) -> anyhow::Result<()> {
        let pane: Arc<dyn Pane> = if let Some(runtime) = self.runtime_backend() {
            runtime.spawn_agent(worktree_path, agent).await?
        } else {
            let client = self.ensure_client().await?;
            let tab = client
                .create_session_terminal(&CreateTerminalOpts {
                    worktree: worktree_selector.to_owned(),
                    launch_agent: Some(agent.to_owned()),
                    ..CreateTerminalOpts::default()
                })
                .await?;
            let terminal = tab.terminal.clone().ok_or_else(|| {
                anyhow::anyhow!("orca terminal is still provisioning; refresh and open it")
            })?;
            self.attach_terminal(
                &client,
                TerminalBinding {
                    terminal,
                    worktree_selector: worktree_selector.to_owned(),
                    worktree_path: worktree_path.to_owned(),
                    parent_tab_id: tab.parent_tab_id.clone(),
                    leaf_id: tab.leaf_id.clone(),
                },
                size,
            )
            .await? as Arc<dyn Pane>
        };
        let mux = Mux::get();
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)?;
        mux.add_tab_to_window(&tab, window_id)?;
        Ok(())
    }

    pub fn activate_runtime_terminal(&self, parent_tab_id: &str, hub_window: WindowId) -> bool {
        let mux = Mux::get();
        let target = mux.iter_panes().into_iter().find_map(|pane| {
            if pane.domain_id() != self.domain_id {
                return None;
            }
            let relay = pane.downcast_ref::<crate::relay_pane::RelayPane>()?;
            (relay.parent_tab_id() == parent_tab_id).then(|| pane.pane_id())
        });
        let Some(pane_id) = target else {
            log::info!("orca: no live pane for parent tab {parent_tab_id}");
            return false;
        };
        let Some((_, _, tab_id)) = mux.resolve_pane_id(pane_id) else {
            return false;
        };
        self.move_tab_to_window(tab_id, hub_window);
        let Some(mut window) = mux.get_window_mut(hub_window) else {
            return false;
        };
        match window.idx_by_id(tab_id) {
            Some(idx) => {
                window.set_active_without_saving(idx);
                drop(window);
                Mux::notify_from_any_thread(MuxNotification::WindowInvalidated(hub_window));
                true
            }
            None => false,
        }
    }

    fn move_tab_to_window(&self, tab_id: TabId, target: WindowId) {
        let mux = Mux::get();
        let current = mux.iter_windows().into_iter().find(|window| {
            mux.get_window(*window)
                .is_some_and(|w| w.idx_by_id(tab_id).is_some())
        });
        let Some(current) = current else {
            return;
        };
        if current == target {
            return;
        }
        let Some(mut window) = mux.get_window_mut(current) else {
            return;
        };
        let Some(idx) = window.idx_by_id(tab_id) else {
            return;
        };
        let tab = window.remove_by_idx(idx);
        drop(window);
        let _ = mux.add_tab_to_window(&tab, target);
    }

    pub async fn hub_connect_ssh(
        &self,
        target_id: &str,
    ) -> anyhow::Result<Option<SshConnectionState>> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().connect_ssh_target(target_id).await;
        }
        Ok(self
            .ensure_client()
            .await?
            .connect_ssh_target(target_id)
            .await?)
    }

    pub async fn hub_open_terminal(&self, parent_tab_id: &str, window: WindowId) -> bool {
        let Some(runtime) = self.runtime_backend() else {
            return false;
        };
        if runtime.has_tab(parent_tab_id) {
            return self.activate_runtime_terminal(parent_tab_id, window);
        }
        runtime.open_tab(parent_tab_id, window).await
    }

    pub fn hub_detach_group(&self, parent_tab_ids: &[String]) {
        if let Some(runtime) = self.runtime_backend() {
            for parent in parent_tab_ids {
                runtime.close_tab(parent);
            }
            return;
        }
        let mux = Mux::get();
        let panes = mux
            .iter_panes()
            .into_iter()
            .filter(|pane| {
                pane.domain_id() == self.domain_id
                    && pane
                        .downcast_ref::<OrcaTerminalPane>()
                        .is_some_and(|orca| parent_tab_ids.contains(&orca.binding().parent_tab_id))
            })
            .map(|pane| pane.pane_id())
            .collect::<Vec<_>>();
        for pane_id in panes {
            mux.remove_pane(pane_id);
        }
    }

    pub async fn hub_create_worktree(&self, repo: &str, name: &str) -> anyhow::Result<()> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().create_worktree(repo, name).await;
        }
        self.ensure_client()
            .await?
            .create_worktree(repo, name)
            .await?;
        Ok(())
    }

    pub async fn hub_browse_dir(&self, path: &str) -> anyhow::Result<ServerDir> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().browse_server_dir(path).await;
        }
        Ok(self.ensure_client().await?.browse_server_dir(path).await?)
    }

    pub async fn hub_add_repo(&self, path: &str) -> anyhow::Result<()> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().add_repo(path).await;
        }
        self.ensure_client().await?.add_repo(path).await?;
        Ok(())
    }

    pub async fn hub_clone_repo(&self, url: &str, destination: &str) -> anyhow::Result<()> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().clone_repo(url, destination).await;
        }
        self.ensure_client()
            .await?
            .clone_repo(url, destination)
            .await?;
        Ok(())
    }

    pub async fn hub_create_repo(&self, parent: &str, name: &str) -> anyhow::Result<()> {
        if let Some(runtime) = self.runtime_backend() {
            return runtime.local_runtime().create_repo(parent, name).await;
        }
        self.ensure_client()
            .await?
            .create_repo(parent, name)
            .await?;
        Ok(())
    }

    pub async fn open_runtime_in_new_window(&self, parent_tab_ids: &[String]) -> bool {
        let Some(runtime) = self.runtime_backend() else {
            return false;
        };
        let builder = Mux::get().new_empty_window(None, None);
        let new_window = *builder;
        let mut opened = false;
        for parent in parent_tab_ids {
            if runtime.has_tab(parent) {
                opened |= self.activate_runtime_terminal(parent, new_window);
            } else {
                opened |= runtime.open_tab(parent, new_window).await;
            }
        }
        drop(builder);
        opened
    }

    pub fn worktree_selector_for_cwd(&self, cwd: &str) -> Option<String> {
        self.worktree_for_cwd(cwd)
            .map(|entry| id_selector(&entry.id))
    }

    fn worktree_for_cwd(&self, cwd: &str) -> Option<WorktreeEntry> {
        let worktrees = self.worktrees.lock();
        worktrees
            .iter()
            .filter(|(path, _)| {
                cwd == path.as_str()
                    || cwd
                        .strip_prefix(path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(path, _)| path.len())
            .map(|(_, entry)| entry.clone())
    }

    fn ensure_layout_publisher(&self) {
        if self.publish_subscribed.swap(true, Ordering::SeqCst) {
            return;
        }
        let applying = self.applying.clone();
        let dirty = self.publish_dirty.clone();
        let order = self.publish_order.clone();
        let running = self.publish_running.clone();
        let domain_id = self.domain_id;
        Mux::get().subscribe(move |notification| {
            let resized = match notification {
                MuxNotification::TabResized(tab_id) => Some(tab_id),
                MuxNotification::WindowInvalidated(_) => None,
                _ => return true,
            };
            if applying.load(Ordering::SeqCst) != 0 {
                return true;
            }
            match resized {
                Some(tab_id) => {
                    dirty.lock().insert(tab_id);
                }
                None => order.store(true, Ordering::SeqCst),
            }
            if !running.swap(true, Ordering::SeqCst) {
                let dirty = dirty.clone();
                let order = order.clone();
                let running = running.clone();
                promise::spawn::spawn_into_main_thread(async move {
                    publish_tab_layouts(domain_id, dirty, order, running).await;
                })
                .detach();
            }
            true
        });
    }

    fn tab_orca_identity(&self, tab: &Arc<Tab>) -> Option<(String, String)> {
        let panes = tab.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            return None;
        }
        let mut identity = None;
        for pos in &panes {
            if pos.pane.domain_id() != self.domain_id {
                return None;
            }
            let orca_pane = pos.pane.downcast_ref::<OrcaTerminalPane>()?;
            let binding = orca_pane.binding();
            if binding.parent_tab_id.is_empty() {
                return None;
            }
            match &identity {
                None => {
                    identity = Some((
                        binding.worktree_selector.clone(),
                        binding.parent_tab_id.clone(),
                    ));
                }
                Some((_, existing)) if *existing == binding.parent_tab_id => {}
                _ => return None,
            }
        }
        identity
    }

    async fn publish_tab_order(&self) -> anyhow::Result<()> {
        let Some(client) = self.client() else {
            return Ok(());
        };
        let Some(window_id) = *self.attach_window.lock() else {
            return Ok(());
        };
        let mut order_by_worktree = Vec::<(String, Vec<String>)>::new();
        {
            let mux = Mux::get();
            let Some(window) = mux.get_window(window_id) else {
                return Ok(());
            };
            for tab in window.iter() {
                let Some((worktree, parent_tab_id)) = self.tab_orca_identity(tab) else {
                    continue;
                };
                match order_by_worktree
                    .iter_mut()
                    .find(|(existing, _)| *existing == worktree)
                {
                    Some((_, ids)) => {
                        if !ids.contains(&parent_tab_id) {
                            ids.push(parent_tab_id);
                        }
                    }
                    None => order_by_worktree.push((worktree, vec![parent_tab_id])),
                }
            }
        }
        for (worktree, mirrored) in order_by_worktree {
            if mirrored.len() < 2 {
                continue;
            }
            let groups = client.list_session_tab_groups(&worktree).await?;
            let Some(group) = groups
                .iter()
                .find(|group| group.tab_order.iter().any(|id| mirrored.contains(id)))
            else {
                continue;
            };
            let current = group
                .tab_order
                .iter()
                .filter(|id| mirrored.contains(id))
                .cloned()
                .collect::<Vec<_>>();
            let desired = mirrored
                .iter()
                .filter(|id| current.contains(id))
                .cloned()
                .collect::<Vec<_>>();
            if desired.len() < 2 || current == desired {
                continue;
            }
            let mut take = desired.iter();
            let next = group
                .tab_order
                .iter()
                .map(|id| {
                    if current.contains(id) {
                        take.next().cloned().unwrap_or_else(|| id.clone())
                    } else {
                        id.clone()
                    }
                })
                .collect::<Vec<_>>();
            let moved = desired
                .iter()
                .zip(&current)
                .find(|(a, b)| a != b)
                .map(|(a, _)| a.clone())
                .unwrap_or_else(|| desired[0].clone());
            client
                .reorder_session_tabs(&worktree, &group.id, &moved, &next)
                .await?;
        }
        Ok(())
    }

    async fn publish_tab_layout(&self, tab_id: TabId) -> anyhow::Result<()> {
        let Some(client) = self.client() else {
            return Ok(());
        };
        let Some(target) = self.publish_target(tab_id) else {
            return Ok(());
        };
        let tabs = client.list_session_tabs(&target.worktree).await?;
        let Some(mut root) = tabs
            .into_iter()
            .filter(|tab| tab.parent_tab_id == target.parent_tab_id)
            .find_map(|tab| tab.parent_layout.and_then(|layout| layout.root))
        else {
            return Ok(());
        };
        if !layout_matches_target(&root, &target) {
            return Ok(());
        }
        if !measure_layout_ratios(&mut root, &target) {
            return Ok(());
        }
        client
            .update_pane_layout(&target.worktree, &target.parent_tab_id, &root)
            .await?;
        Ok(())
    }

    fn publish_target(&self, tab_id: TabId) -> Option<PublishTarget> {
        let tab = Mux::get().get_tab(tab_id)?;
        let panes = tab.iter_panes_ignoring_zoom();
        if panes.len() < 2 {
            return None;
        }
        let mut worktree = None;
        let mut parent_tab_id = None;
        let mut leaf_panes = HashMap::new();
        let mut rects = HashMap::new();
        let mut pane_order = Vec::with_capacity(panes.len());
        for pos in &panes {
            if pos.pane.domain_id() != self.domain_id {
                return None;
            }
            let orca_pane = pos.pane.downcast_ref::<OrcaTerminalPane>()?;
            if orca_pane.is_held() {
                return None;
            }
            let binding = orca_pane.binding();
            if binding.parent_tab_id.is_empty() || binding.leaf_id.is_empty() {
                return None;
            }
            match &parent_tab_id {
                None => {
                    worktree = Some(binding.worktree_selector.clone());
                    parent_tab_id = Some(binding.parent_tab_id.clone());
                }
                Some(existing) if *existing == binding.parent_tab_id => {}
                _ => return None,
            }
            let pane_id = pos.pane.pane_id();
            leaf_panes.insert(binding.leaf_id.clone(), pane_id);
            rects.insert(
                pane_id,
                PaneRect {
                    left: pos.left,
                    top: pos.top,
                    width: pos.width,
                    height: pos.height,
                },
            );
            pane_order.push(pane_id);
        }
        Some(PublishTarget {
            worktree: worktree?,
            parent_tab_id: parent_tab_id?,
            leaf_panes,
            rects,
            pane_order,
            split_directions: tab
                .iter_splits()
                .iter()
                .map(|split| split.direction)
                .collect(),
        })
    }

    pub async fn ensure_client(&self) -> anyhow::Result<OrcaClient> {
        if let Some(client) = self.client() {
            return Ok(client);
        }
        self.ensure_layout_publisher();
        let client = match &self.target {
            RuntimeTarget::Direct(offer) => OrcaClient::connect(offer).await?,
            RuntimeTarget::Ssh { target } => {
                let target = target.clone();
                let offer = self.resolve_ssh_offer(&target, false).await?;
                match OrcaClient::connect(&offer).await {
                    Ok(client) => client,
                    Err(err) => {
                        log::info!(
                            "orca pairing for {target} was rejected ({err:#}); \
                             requesting a fresh offer"
                        );
                        let offer = self.resolve_ssh_offer(&target, true).await?;
                        OrcaClient::connect(&offer).await?
                    }
                }
            }
        };
        let connection = self.connection.fetch_add(1, Ordering::SeqCst) + 1;
        *self.client.lock() = Some(client.clone());
        *self.state.lock() = DomainState::Attached;
        {
            let client = client.clone();
            let slot = self.client.clone();
            let state = self.state.clone();
            let generation = self.connection.clone();
            let domain_id = self.domain_id;
            promise::spawn::spawn(async move {
                let _ = client.run().await;
                if generation.load(Ordering::SeqCst) != connection {
                    return;
                }
                *slot.lock() = None;
                *state.lock() = DomainState::Detached;
                let mut delay = RECONNECT_MIN_DELAY;
                loop {
                    smol::Timer::after(delay).await;
                    let Some(domain) = Mux::get().get_domain(domain_id) else {
                        return;
                    };
                    let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                        return;
                    };
                    if orca.client().is_some() || !orca.wants_connection() {
                        return;
                    }
                    if orca.ensure_client().await.is_ok() {
                        return;
                    }
                    delay = (delay * 2).min(RECONNECT_MAX_DELAY);
                }
            })
            .detach();
        }
        {
            let client = client.clone();
            let domain_id = self.domain_id;
            promise::spawn::spawn(async move {
                let Some(domain) = Mux::get().get_domain(domain_id) else {
                    return;
                };
                let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                    return;
                };
                if let Err(err) = orca.revive_panes(&client).await {
                    log::warn!("orca domain pane revival failed: {err:#}");
                }
            })
            .detach();
        }
        {
            let client = client.clone();
            let domain_id = self.domain_id;
            promise::spawn::spawn(async move {
                let Ok(events) = client.subscribe_session_tabs_all().await else {
                    return;
                };
                while let Ok(event) = events.recv_async().await {
                    let mut statuses = HashMap::new();
                    collect_agent_statuses(&event, &mut statuses);
                    while let Ok(event) = events.try_recv() {
                        collect_agent_statuses(&event, &mut statuses);
                    }
                    let Some(domain) = Mux::get().get_domain(domain_id) else {
                        break;
                    };
                    let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                        break;
                    };
                    orca.apply_agent_statuses(&statuses);
                    if let Err(err) = orca.sync_layouts(&client).await {
                        log::warn!("orca domain layout sync failed: {err:#}");
                    }
                }
            })
            .detach();
        }
        {
            let client = client.clone();
            let worktrees = self.worktrees.clone();
            promise::spawn::spawn(async move {
                if let Ok(records) = client.list_worktrees().await {
                    let mut worktrees = worktrees.lock();
                    for record in records {
                        worktrees.insert(
                            record.git.path.clone(),
                            WorktreeEntry {
                                id: record.id,
                                display_name: record.display_name,
                                path: record.git.path.clone(),
                            },
                        );
                    }
                }
            })
            .detach();
        }
        Ok(client)
    }

    async fn refresh_worktrees(&self, client: &OrcaClient) -> anyhow::Result<()> {
        let records = client.list_worktrees().await?;
        let mut worktrees = self.worktrees.lock();
        for record in records {
            worktrees.insert(
                record.git.path.clone(),
                WorktreeEntry {
                    id: record.id,
                    display_name: record.display_name,
                    path: record.git.path.clone(),
                },
            );
        }
        Ok(())
    }

    pub async fn attach_terminal(
        &self,
        client: &OrcaClient,
        binding: TerminalBinding,
        size: TerminalSize,
    ) -> anyhow::Result<Arc<OrcaTerminalPane>> {
        if !self
            .attached_terminals
            .lock()
            .insert(binding.terminal.clone())
        {
            anyhow::bail!("terminal {} is already attached", binding.terminal);
        }
        let handle = match client
            .subscribe_terminal(&binding.terminal, size.rows as u32, size.cols as u32)
            .await
        {
            Ok(handle) => handle,
            Err(err) => {
                self.attached_terminals.lock().remove(&binding.terminal);
                return Err(err.into());
            }
        };
        let (pane, input_rx) = OrcaTerminalPane::new(
            alloc_pane_id(),
            self.domain_id,
            binding,
            size,
            handle.writer(),
            client.clone(),
        );
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        Mux::get().add_pane(&pane_dyn)?;
        pane.start_io(handle, input_rx);
        Ok(pane)
    }

    pub(crate) fn forget_terminal(&self, terminal: &str) {
        self.attached_terminals.lock().remove(terminal);
    }

    async fn realise_split_ops(
        &self,
        client: &OrcaClient,
        tab: &Arc<Tab>,
        root: &VisualPaneNode,
        pane_ids: &mut HashMap<String, mux::pane::PaneId>,
        pool: &mut HashMap<String, Arc<dyn Pane>>,
        ctx: &RealiseContext<'_>,
    ) -> anyhow::Result<()> {
        for op in plan_splits(root) {
            if pane_ids.contains_key(&op.insert) {
                continue;
            }
            let Some(&from_pane_id) = pane_ids.get(&op.split_from) else {
                continue;
            };
            let Some(pane_index) = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .find(|p| p.pane.pane_id() == from_pane_id)
                .map(|p| p.index)
            else {
                continue;
            };
            let request = SplitRequest {
                direction: op.direction,
                target_is_second: true,
                top_level: false,
                size: MuxSplitSize::Percent(split_percent(ctx.ratios.get(&op.insert).copied())),
            };
            let Some(split_size) = tab.compute_split_size(pane_index, request) else {
                continue;
            };
            let pane_dyn = if let Some(pooled) = pool.remove(&op.insert) {
                pane_ids.insert(op.insert.clone(), pooled.pane_id());
                pooled
            } else if let Some(&live_id) = ctx.live.get(&op.insert) {
                let Some(moved) = detach_live_pane(live_id) else {
                    continue;
                };
                pane_ids.insert(op.insert.clone(), live_id);
                moved
            } else {
                let Some(summary) = ctx
                    .summaries
                    .get(&op.insert)
                    .filter(|summary| summary.pty_id.is_some() && !summary.orphaned)
                else {
                    continue;
                };
                if self.attached_terminals.lock().contains(&op.insert) {
                    continue;
                }
                let new_pane = self
                    .attach_terminal(
                        client,
                        TerminalBinding::from_summary(summary),
                        split_size.second,
                    )
                    .await?;
                pane_ids.insert(op.insert.clone(), new_pane.pane_id());
                new_pane as Arc<dyn Pane>
            };
            tab.split_and_insert(pane_index, request, pane_dyn)?;
        }
        Ok(())
    }

    async fn gather_stray_members(
        &self,
        client: &OrcaClient,
        tab: &Arc<Tab>,
        handles: &[String],
        pane_ids: &mut HashMap<String, mux::pane::PaneId>,
        ctx: &RealiseContext<'_>,
    ) -> anyhow::Result<()> {
        for handle in handles {
            if pane_ids.contains_key(handle) {
                continue;
            }
            let Some(anchor_index) = tab.iter_panes_ignoring_zoom().first().map(|p| p.index) else {
                continue;
            };
            let request = SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                top_level: false,
                size: MuxSplitSize::Percent(50),
            };
            let Some(split_size) = tab.compute_split_size(anchor_index, request) else {
                continue;
            };
            let pane_dyn: Arc<dyn Pane> = if let Some(&live_id) = ctx.live.get(handle) {
                let Some(moved) = detach_live_pane(live_id) else {
                    continue;
                };
                pane_ids.insert(handle.clone(), live_id);
                moved
            } else {
                let Some(summary) = ctx
                    .summaries
                    .get(handle)
                    .filter(|summary| summary.pty_id.is_some() && !summary.orphaned)
                else {
                    continue;
                };
                if self.attached_terminals.lock().contains(handle) {
                    continue;
                }
                let pane = self
                    .attach_terminal(
                        client,
                        TerminalBinding::from_summary(summary),
                        split_size.second,
                    )
                    .await?;
                pane_ids.insert(handle.clone(), pane.pane_id());
                pane
            };
            tab.split_and_insert(anchor_index, request, pane_dyn)?;
        }
        Ok(())
    }

    fn tab_structure_matches(
        tab: &Arc<Tab>,
        root: &VisualPaneNode,
        pane_ids: &HashMap<String, mux::pane::PaneId>,
    ) -> bool {
        let desired = pane_tree_handles(root);
        let panes = tab.iter_panes_ignoring_zoom();
        if panes.len() != desired.len() {
            return false;
        }
        for (position, handle) in desired.iter().enumerate() {
            if pane_ids.get(handle).copied() != panes.get(position).map(|p| p.pane.pane_id()) {
                return false;
            }
        }
        let ops = plan_splits(root);
        let splits = tab.iter_splits();
        splits.len() == ops.len()
            && ops
                .iter()
                .zip(&splits)
                .all(|(op, split)| split.direction == op.direction)
    }

    async fn rebuild_tab(
        &self,
        client: &OrcaClient,
        tab: &Arc<Tab>,
        root: &VisualPaneNode,
        pane_ids: &mut HashMap<String, mux::pane::PaneId>,
        ctx: &RealiseContext<'_>,
    ) -> anyhow::Result<()> {
        let order = pane_tree_handles(root);
        let Some(first) = order.first() else {
            return Ok(());
        };
        tab.set_zoomed(false);
        let mut pool = HashMap::new();
        for handle in order.iter().skip(1) {
            let Some(&pane_id) = pane_ids.get(handle) else {
                continue;
            };
            if let Some(pane) = tab.remove_pane(pane_id) {
                pool.insert(handle.clone(), pane);
            }
        }
        pane_ids.retain(|handle, _| handle == first);
        self.realise_split_ops(client, tab, root, pane_ids, &mut pool, ctx)
            .await
    }

    async fn normalise_visual_tab(
        &self,
        client: &OrcaClient,
        vtab: &VisualTab,
        ctx: &RealiseContext<'_>,
    ) -> anyhow::Result<()> {
        let handles = pane_tree_handles(&vtab.panes);
        let mux = Mux::get();
        let mut pane_ids = HashMap::new();
        let mut tab_ids = HashSet::new();
        for handle in &handles {
            let Some(&pane_id) = ctx.live.get(handle) else {
                return Ok(());
            };
            let Some((_, _, tab_id)) = mux.resolve_pane_id(pane_id) else {
                return Ok(());
            };
            pane_ids.insert(handle.clone(), pane_id);
            tab_ids.insert(tab_id);
        }
        if tab_ids.len() != 1 {
            return Ok(());
        }
        let Some(tab) = tab_ids
            .iter()
            .next()
            .and_then(|tab_id| mux.get_tab(*tab_id))
        else {
            return Ok(());
        };
        if tab.iter_panes_ignoring_zoom().len() != handles.len() {
            return Ok(());
        }
        if !Self::tab_structure_matches(&tab, &vtab.panes, &pane_ids) {
            self.rebuild_tab(client, &tab, &vtab.panes, &mut pane_ids, ctx)
                .await?;
        }
        Self::apply_split_ratios(&tab, &vtab.panes, &pane_ids, ctx.ratios);
        Ok(())
    }

    fn apply_split_ratios(
        tab: &Arc<Tab>,
        root: &VisualPaneNode,
        pane_ids: &HashMap<String, mux::pane::PaneId>,
        ratios: &HashMap<String, f64>,
    ) {
        if !Self::tab_structure_matches(tab, root, pane_ids) {
            return;
        }
        let ops = plan_splits(root);
        for (index, op) in ops.iter().enumerate() {
            let Some(&ratio) = ratios.get(&op.insert) else {
                continue;
            };
            let panes = tab.iter_panes_ignoring_zoom();
            let extent = |handles: &[String]| -> Option<(usize, usize)> {
                let mut lo = usize::MAX;
                let mut hi = 0usize;
                for handle in handles {
                    let pane_id = pane_ids.get(handle)?;
                    let pane = panes.iter().find(|p| p.pane.pane_id() == *pane_id)?;
                    let (start, len) = match op.direction {
                        SplitDirection::Horizontal => (pane.left, pane.width),
                        SplitDirection::Vertical => (pane.top, pane.height),
                    };
                    lo = lo.min(start);
                    hi = hi.max(start + len);
                }
                (lo < hi).then_some((lo, hi - lo))
            };
            let Some((_, first_extent)) = extent(&op.first_handles) else {
                continue;
            };
            let Some((_, second_extent)) = extent(&op.second_handles) else {
                continue;
            };
            let node_extent = first_extent + second_extent + 1;
            if node_extent < 4 {
                continue;
            }
            let desired_first = ((ratio.clamp(0.01, 0.99) * (node_extent - 1) as f64).round()
                as isize)
                .clamp(1, node_extent as isize - 2);
            let delta = desired_first - first_extent as isize;
            if delta != 0 {
                tab.resize_split_by(index, delta);
            }
        }
    }

    async fn worktree_ratios(
        &self,
        client: &OrcaClient,
        worktree_id: &str,
        summaries: &HashMap<String, TerminalSummary>,
    ) -> HashMap<String, f64> {
        let Ok(tabs) = client.list_session_tabs(&id_selector(worktree_id)).await else {
            return HashMap::new();
        };
        let leaf_to_handle = summaries
            .values()
            .filter(|summary| summary.worktree_id == worktree_id)
            .map(|summary| (summary.leaf_id.clone(), summary.handle.clone()))
            .collect::<HashMap<_, _>>();
        let mut by_leaf = HashMap::new();
        let mut seen = HashSet::new();
        for tab in tabs {
            if tab.kind != "terminal" || !seen.insert(tab.parent_tab_id.clone()) {
                continue;
            }
            if let Some(root) = tab.parent_layout.as_ref().and_then(|l| l.root.as_ref()) {
                collect_layout_ratios(root, &mut by_leaf);
            }
        }
        by_leaf
            .into_iter()
            .filter_map(|(leaf, ratio)| leaf_to_handle.get(&leaf).map(|h| (h.clone(), ratio)))
            .collect()
    }

    pub(crate) fn apply_agent_statuses(&self, statuses: &HashMap<String, Option<String>>) {
        if statuses.is_empty() {
            return;
        }
        for pane in Mux::get().iter_panes() {
            if pane.domain_id() != self.domain_id {
                continue;
            }
            let Some(orca_pane) = pane.downcast_ref::<OrcaTerminalPane>() else {
                continue;
            };
            let Some(state) = statuses.get(&orca_pane.binding().terminal) else {
                continue;
            };
            if orca_pane.set_agent_state(state.clone()) {
                Mux::notify_from_any_thread(mux::MuxNotification::PaneOutput(pane.pane_id()));
            }
        }
    }

    fn wants_connection(&self) -> bool {
        if self.attach_window.lock().is_some() {
            return true;
        }
        Mux::get().iter_panes().into_iter().any(|pane| {
            pane.domain_id() == self.domain_id
                && pane
                    .downcast_ref::<OrcaTerminalPane>()
                    .is_some_and(|orca_pane| orca_pane.is_held())
        })
    }

    async fn revive_panes(&self, client: &OrcaClient) -> anyhow::Result<()> {
        let _topology = self.topology.lock().await;
        let summaries = client
            .list_terminals(None)
            .await?
            .into_iter()
            .map(|summary| (summary.handle.clone(), summary))
            .collect::<HashMap<_, _>>();
        let held = Mux::get()
            .iter_panes()
            .into_iter()
            .filter(|pane| pane.domain_id() == self.domain_id)
            .collect::<Vec<_>>();
        for pane in held {
            let Some(orca_pane) = pane.downcast_ref::<OrcaTerminalPane>() else {
                continue;
            };
            if !orca_pane.is_held() {
                continue;
            }
            let terminal = orca_pane.binding().terminal.clone();
            if summaries
                .get(&terminal)
                .is_some_and(|summary| summary.pty_id.is_some() && !summary.orphaned)
            {
                let size = orca_pane.size();
                match client
                    .subscribe_terminal(&terminal, size.rows as u32, size.cols as u32)
                    .await
                {
                    Ok(handle) => orca_pane.resume(handle),
                    Err(err) => log::warn!("orca: could not revive {terminal}: {err:#}"),
                }
            } else {
                orca_pane.declare_dead();
            }
        }
        Ok(())
    }

    async fn materialise_visual_tab(
        &self,
        client: &OrcaClient,
        vtab: &VisualTab,
        ctx: &RealiseContext<'_>,
        size: TerminalSize,
        window_id: WindowId,
    ) -> anyhow::Result<()> {
        let first = leftmost_terminal(&vtab.panes).to_owned();
        let mut pane_ids = HashMap::new();
        let first_pane = if let Some(&live_id) = ctx.live.get(&first) {
            let Some(moved) = detach_live_pane(live_id) else {
                return Ok(());
            };
            pane_ids.insert(first, live_id);
            moved
        } else {
            let Some(summary) = ctx.summaries.get(&first) else {
                return Ok(());
            };
            if self.attached_terminals.lock().contains(&first) {
                return Ok(());
            }
            let pane = self
                .attach_terminal(client, TerminalBinding::from_summary(summary), size)
                .await?;
            pane_ids.insert(first, pane.pane_id());
            pane as Arc<dyn Pane>
        };

        let mux = Mux::get();
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&first_pane);
        mux.add_tab_and_active_pane(&tab)?;
        mux.add_tab_to_window(&tab, window_id)?;

        self.realise_split_ops(
            client,
            &tab,
            &vtab.panes,
            &mut pane_ids,
            &mut HashMap::new(),
            ctx,
        )
        .await?;
        Self::apply_split_ratios(&tab, &vtab.panes, &pane_ids, ctx.ratios);
        Ok(())
    }

    fn live_panes(&self) -> HashMap<String, mux::pane::PaneId> {
        Mux::get()
            .iter_panes()
            .into_iter()
            .filter_map(|pane| {
                if pane.domain_id() != self.domain_id {
                    return None;
                }
                let orca = pane.downcast_ref::<OrcaTerminalPane>()?;
                Some((orca.binding().terminal.clone(), pane.pane_id()))
            })
            .collect()
    }

    fn sync_tab_order(&self, layouts: &[VisualLayout], live: &HashMap<String, mux::pane::PaneId>) {
        let Some(window_id) = *self.attach_window.lock() else {
            return;
        };
        let mux = Mux::get();
        let mut desired = Vec::new();
        let mut seen = HashSet::new();
        for layout in layouts {
            for vtab in layout.root.tabs() {
                let found = pane_tree_handles(&vtab.panes).iter().find_map(|handle| {
                    let (_, _, tab_id) = mux.resolve_pane_id(*live.get(handle)?)?;
                    Some(tab_id)
                });
                if let Some(tab_id) = found {
                    if seen.insert(tab_id) {
                        desired.push(tab_id);
                    }
                }
            }
        }
        if desired.len() < 2 {
            return;
        }
        let Some(mut window) = mux.get_window_mut(window_id) else {
            return;
        };
        let desired = desired
            .into_iter()
            .filter(|tab_id| window.idx_by_id(*tab_id).is_some())
            .collect::<Vec<_>>();
        let active = window.get_active().map(|tab| tab.tab_id());
        for position in 0..desired.len() {
            let slot = window
                .iter()
                .enumerate()
                .filter(|(_, tab)| desired.contains(&tab.tab_id()))
                .map(|(idx, _)| idx)
                .nth(position);
            let current = window.idx_by_id(desired[position]);
            let (Some(slot), Some(current)) = (slot, current) else {
                break;
            };
            if current != slot {
                let tab = window.remove_by_idx(current);
                window.insert(slot, &tab);
            }
        }
        if let Some(active_id) = active {
            if let Some(idx) = window.idx_by_id(active_id) {
                window.set_active_without_saving(idx);
            }
        }
    }

    pub async fn sync_layouts(&self, client: &OrcaClient) -> anyhow::Result<()> {
        let _topology = self.topology.lock().await;
        let _applying = ApplyGuard::hold(&self.applying);
        let list = client.list_terminals_with_layouts(None).await?;
        let summaries = list
            .terminals
            .into_iter()
            .map(|summary| (summary.handle.clone(), summary))
            .collect::<HashMap<_, _>>();
        let mux = Mux::get();
        let live = self.live_panes();
        let attach_window = *self.attach_window.lock();
        let mut layouts = list.visual_layouts;
        layouts.sort_by(|a, b| a.worktree_path.cmp(&b.worktree_path));

        let mut layout_ratios = Vec::with_capacity(layouts.len());
        for layout in &layouts {
            layout_ratios.push(
                self.worktree_ratios(client, &layout.worktree_id, &summaries)
                    .await,
            );
        }

        for (layout, ratios) in layouts.iter().zip(&layout_ratios) {
            let ctx = RealiseContext {
                summaries: &summaries,
                ratios,
                live: &live,
            };
            for vtab in layout.root.tabs() {
                let handles = pane_tree_handles(&vtab.panes);
                let has_new = handles.iter().any(|handle| {
                    !live.contains_key(handle)
                        && !self.attached_terminals.lock().contains(handle)
                        && summaries
                            .get(handle)
                            .is_some_and(|summary| summary.pty_id.is_some() && !summary.orphaned)
                });
                let member_tabs = handles
                    .iter()
                    .filter_map(|handle| {
                        let pane_id = live.get(handle).copied()?;
                        let (_, _, tab_id) = mux.resolve_pane_id(pane_id)?;
                        Some((handle.clone(), pane_id, tab_id))
                    })
                    .collect::<Vec<_>>();

                let Some(target_tab_id) = majority_tab(&member_tabs) else {
                    if !has_new {
                        continue;
                    }
                    let Some(window_id) = attach_window else {
                        continue;
                    };
                    if mux.get_window(window_id).is_none() {
                        continue;
                    }
                    self.materialise_visual_tab(
                        client,
                        vtab,
                        &ctx,
                        Self::attach_size(Some(window_id)),
                        window_id,
                    )
                    .await?;
                    continue;
                };

                let Some(tab) = mux.get_tab(target_tab_id) else {
                    continue;
                };
                let mut pane_ids = member_tabs
                    .iter()
                    .filter(|(_, _, tab_id)| *tab_id == target_tab_id)
                    .map(|(handle, pane_id, _)| (handle.clone(), *pane_id))
                    .collect::<HashMap<_, _>>();
                let misplaced = member_tabs
                    .iter()
                    .any(|(_, _, tab_id)| *tab_id != target_tab_id);
                if has_new || misplaced {
                    self.realise_split_ops(
                        client,
                        &tab,
                        &vtab.panes,
                        &mut pane_ids,
                        &mut HashMap::new(),
                        &ctx,
                    )
                    .await?;
                    self.gather_stray_members(client, &tab, &handles, &mut pane_ids, &ctx)
                        .await?;
                }
            }
        }

        let live = self.live_panes();
        for (layout, ratios) in layouts.iter().zip(&layout_ratios) {
            let ctx = RealiseContext {
                summaries: &summaries,
                ratios,
                live: &live,
            };
            for vtab in layout.root.tabs() {
                self.normalise_visual_tab(client, vtab, &ctx).await?;
            }
        }

        self.sync_tab_order(&layouts, &live);
        Ok(())
    }

    async fn split_binding(
        &self,
        client: &OrcaClient,
        handle: &str,
        source: &crate::pane::TerminalBinding,
    ) -> TerminalBinding {
        let summary = client
            .list_terminals(Some(&source.worktree_selector))
            .await
            .ok()
            .and_then(|terminals| {
                terminals
                    .into_iter()
                    .find(|summary| summary.handle == handle)
            });
        let (parent_tab_id, leaf_id) = summary
            .map(|summary| (summary.tab_id, summary.leaf_id))
            .unwrap_or_default();
        TerminalBinding {
            terminal: handle.to_owned(),
            worktree_selector: source.worktree_selector.clone(),
            worktree_path: source.worktree_path.clone(),
            parent_tab_id,
            leaf_id,
        }
    }

    fn attach_size(window_id: Option<WindowId>) -> TerminalSize {
        let mux = Mux::get();
        window_id
            .and_then(|id| {
                let window = mux.get_window(id)?;
                let tab = window.get_active()?;
                Some(tab.get_size())
            })
            .unwrap_or(TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            })
    }
}

#[async_trait(?Send)]
impl Domain for OrcaDomain {
    async fn spawn_pane(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if self.runtime_mode.load(Ordering::SeqCst) {
            let runtime = self.runtime.lock().clone();
            if let Some(runtime) = runtime {
                return runtime.spawn_pane(size, command_dir).await;
            }
        }
        if self.relay_mode.load(Ordering::SeqCst) {
            return self.relay.spawn_pane(size, command_dir).await;
        }
        let client = self.ensure_client().await?;
        let cwd = command_dir.ok_or_else(|| {
            anyhow::anyhow!("orca domain {} needs a working directory", self.name)
        })?;
        let entry = match self.worktree_for_cwd(&cwd) {
            Some(entry) => entry,
            None => {
                self.refresh_worktrees(&client).await?;
                self.worktree_for_cwd(&cwd).ok_or_else(|| {
                    anyhow::anyhow!("no orca worktree contains {cwd}; add it in orca first")
                })?
            }
        };
        let worktree = id_selector(&entry.id);

        let command = command.map(|builder| {
            let argv = builder
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            shell_words::join(argv.iter().map(String::as_str))
        });

        let _topology = self.topology.lock().await;
        let tab = client
            .create_session_terminal(&CreateTerminalOpts {
                worktree: worktree.clone(),
                command,
                ..CreateTerminalOpts::default()
            })
            .await?;
        let terminal = tab.terminal.clone().ok_or_else(|| {
            anyhow::anyhow!("orca terminal is still provisioning; try again shortly")
        })?;

        let pane = self
            .attach_terminal(
                &client,
                TerminalBinding {
                    terminal,
                    worktree_selector: worktree,
                    worktree_path: entry.path,
                    parent_tab_id: tab.parent_tab_id.clone(),
                    leaf_id: tab.leaf_id.clone(),
                },
                size,
            )
            .await?;
        Ok(pane as Arc<dyn Pane>)
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        tab: mux::tab::TabId,
        pane_id: mux::pane::PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if self.runtime_mode.load(Ordering::SeqCst) {
            let runtime = self.runtime.lock().clone();
            if let Some(runtime) = runtime {
                return runtime
                    .split_pane(source, tab, pane_id, split_request)
                    .await;
            }
        }
        if self.relay_mode.load(Ordering::SeqCst) {
            return self
                .relay
                .split_pane(source, tab, pane_id, split_request)
                .await;
        }
        let mux = Mux::get();
        let tab = mux
            .get_tab(tab)
            .ok_or_else(|| anyhow::anyhow!("invalid tab id {tab}"))?;
        let source_pane = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
            .map(|p| p.pane.clone())
            .ok_or_else(|| anyhow::anyhow!("invalid pane id {pane_id}"))?;

        let (command, command_dir) = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => (command, command_dir),
            SplitSource::MovePane(_) => {
                anyhow::bail!("moving panes into an orca domain is not supported")
            }
        };

        let Some(orca_pane) = source_pane.downcast_ref::<OrcaTerminalPane>() else {
            let cwd = command_dir.or_else(|| {
                source_pane
                    .get_current_working_dir(mux::pane::CachePolicy::FetchImmediate)
                    .filter(|url| url.scheme() == "file")
                    .map(|url| url.path().to_owned())
            });
            let pane_index = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .find(|p| p.pane.pane_id() == pane_id)
                .map(|p| p.index)
                .ok_or_else(|| anyhow::anyhow!("invalid pane id {pane_id}"))?;
            let split_size = tab
                .compute_split_size(pane_index, split_request)
                .ok_or_else(|| anyhow::anyhow!("invalid pane index {pane_index}"))?;
            let pane = self.spawn_pane(split_size.second, command, cwd).await?;
            tab.split_and_insert(pane_index, split_request, pane.clone())?;
            return Ok(pane);
        };

        let client = self.ensure_client().await?;
        let _topology = self.topology.lock().await;
        let _applying = ApplyGuard::hold(&self.applying);
        let command = command.map(|builder| {
            let argv = builder
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            shell_words::join(argv.iter().map(String::as_str))
        });
        let direction = match split_request.direction {
            SplitDirection::Horizontal => OrcaSplitDirection::Horizontal,
            SplitDirection::Vertical => OrcaSplitDirection::Vertical,
        };
        let split = client
            .split_terminal(&orca_pane.binding().terminal, direction, command.as_deref())
            .await?;
        let binding = self
            .split_binding(&client, &split.handle, orca_pane.binding())
            .await;

        let pane_index = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
            .map(|p| p.index)
            .ok_or_else(|| anyhow::anyhow!("invalid pane id {pane_id}"))?;
        let split_size = tab
            .compute_split_size(pane_index, split_request)
            .ok_or_else(|| anyhow::anyhow!("invalid pane index {pane_index}"))?;
        let pane = self
            .attach_terminal(&client, binding, split_size.second)
            .await?;
        let pane_dyn = pane as Arc<dyn Pane>;
        tab.split_and_insert(pane_index, split_request, pane_dyn.clone())?;
        Ok(pane_dyn)
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
        if let RuntimeTarget::Ssh { target } = &self.target {
            if self.ssh_attach_claimed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(runtime) =
                crate::runtime_backend::RuntimeBackend::discover(self.domain_id, &self.name, target)
                    .await
            {
                match runtime.attach(window_id).await {
                    Ok(()) => {
                        *self.runtime.lock() = Some(runtime);
                        self.runtime_mode.store(true, Ordering::SeqCst);
                        *self.attach_window.lock() = window_id;
                        *self.state.lock() = DomainState::Attached;
                        log::info!("orca domain {} using local-runtime mode", self.name);
                        return Ok(());
                    }
                    Err(err) => {
                        log::warn!(
                            "orca domain {} local-runtime attach failed ({err:#}); \
                             falling back",
                            self.name
                        );
                    }
                }
            }
            if crate::ssh::has_live_relay(target).await {
                self.relay_mode.store(true, Ordering::SeqCst);
                match self.relay.attach(window_id).await {
                    Ok(()) => {
                        *self.attach_window.lock() = window_id;
                        *self.state.lock() = DomainState::Attached;
                        return Ok(());
                    }
                    Err(err) => {
                        log::warn!(
                            "orca domain {} relay attach failed ({err:#}); falling back to orcad",
                            self.name
                        );
                        self.relay_mode.store(false, Ordering::SeqCst);
                        self.relay.detach();
                    }
                }
            }
        }
        log::info!("orca domain {} connecting", self.name);
        let client = self.ensure_client().await?;
        self.refresh_worktrees(&client).await?;
        log::info!(
            "orca domain {} connected to runtime {}",
            self.name,
            client.runtime_id()
        );

        let mux = Mux::get();
        let size = Self::attach_size(window_id);
        let window_id = match window_id {
            Some(window_id) => window_id,
            None => *mux.new_empty_window(None, None),
        };
        *self.attach_window.lock() = Some(window_id);

        let _topology = self.topology.lock().await;
        let _applying = ApplyGuard::hold(&self.applying);
        let list = client.list_terminals_with_layouts(None).await?;
        let mut summaries = list
            .terminals
            .into_iter()
            .map(|summary| (summary.handle.clone(), summary))
            .collect::<HashMap<_, _>>();
        let mut layouts = list.visual_layouts;
        layouts.sort_by(|a, b| a.worktree_path.cmp(&b.worktree_path));
        log::info!(
            "orca domain {} attaching {} terminals",
            self.name,
            summaries.len()
        );

        let no_live = HashMap::new();
        for layout in &layouts {
            let ratios = self
                .worktree_ratios(&client, &layout.worktree_id, &summaries)
                .await;
            for vtab in layout.root.tabs() {
                let handles = pane_tree_handles(&vtab.panes);
                let attachable = !handles.is_empty()
                    && handles.iter().all(|handle| {
                        summaries
                            .get(handle)
                            .is_some_and(|summary| summary.pty_id.is_some() && !summary.orphaned)
                            && !self.attached_terminals.lock().contains(handle)
                    });
                if !attachable {
                    continue;
                }

                let ctx = RealiseContext {
                    summaries: &summaries,
                    ratios: &ratios,
                    live: &no_live,
                };
                self.materialise_visual_tab(&client, vtab, &ctx, size, window_id)
                    .await?;

                for handle in handles {
                    summaries.remove(&handle);
                }
            }
        }

        let mut leftovers = summaries.into_values().collect::<Vec<_>>();
        leftovers.sort_by(|a, b| {
            a.worktree_path
                .cmp(&b.worktree_path)
                .then_with(|| a.handle.cmp(&b.handle))
        });
        for summary in leftovers {
            if summary.pty_id.is_none() || summary.orphaned {
                continue;
            }
            if self.attached_terminals.lock().contains(&summary.handle) {
                continue;
            }
            let pane = self
                .attach_terminal(&client, TerminalBinding::from_summary(&summary), size)
                .await?;

            let pane_dyn: Arc<dyn Pane> = pane as Arc<dyn Pane>;
            let tab = Arc::new(Tab::new(&size));
            tab.assign_pane(&pane_dyn);
            mux.add_tab_and_active_pane(&tab)?;
            mux.add_tab_to_window(&tab, window_id)?;
        }

        self.sync_tab_order(&layouts, &self.live_panes());
        *self.state.lock() = DomainState::Attached;
        Ok(())
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.ssh_attach_claimed.store(false, Ordering::SeqCst);
        if self.runtime_mode.swap(false, Ordering::SeqCst) {
            if let Some(runtime) = self.runtime.lock().take() {
                runtime.detach();
            }
            *self.attach_window.lock() = None;
            *self.state.lock() = DomainState::Detached;
            return Ok(());
        }
        if self.relay_mode.swap(false, Ordering::SeqCst) {
            self.relay.detach();
            *self.attach_window.lock() = None;
            *self.state.lock() = DomainState::Detached;
            return Ok(());
        }
        self.reset_client();
        self.attached_terminals.lock().clear();
        *self.attach_window.lock() = None;
        self.stop_tunnel();
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
        Ok(())
    }

    fn state(&self) -> DomainState {
        *self.state.lock()
    }
}
