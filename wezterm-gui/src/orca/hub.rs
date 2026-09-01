use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::anyhow;
use config::keyassignment::{OrcaHubArgs, SplitSize};
use mux::domain::{Domain, DomainId, SplitSource};
use mux::pane::{
    alloc_pane_id, impl_for_each_logical_line_via_get_logical_lines,
    impl_get_logical_lines_via_get_lines, CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane,
    PaneId, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{SplitDirection, SplitRequest, SplitSize as MuxSplitSize, Tab as MuxTab};
use mux::window::WindowId;
use mux::Mux;
use orca_client::{id_selector, CreateTerminalOpts, OrcaClient, PairingOffer, RuntimeEvent};
use orca_mux::{OrcaDomain, TerminalBinding};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use termwiz::cell::CellAttributes;
use termwiz::color::AnsiColor;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{CursorVisibility, Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};
use window::{Window, WindowOps};

use crate::paseo::agent::{attr_bold, attr_bold_fg, attr_default, attr_dim, attr_fg, make_line};
use crate::termwindow::TermWindow;

const RECONNECT_MIN_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct TerminalRow {
    handle: String,
    parent_tab_id: String,
    leaf_id: String,
    worktree_selector: String,
    worktree_path: String,
    title: String,
    agent: Option<String>,
    connected: bool,
}

#[derive(Clone)]
struct WorktreeGroup {
    selector: String,
    cwd: String,
    display: String,
    branch: String,
    status: String,
    folded: bool,
    terminals: Vec<TerminalRow>,
}

#[derive(Clone, Copy, PartialEq)]
enum RowKind {
    Group(usize),
    Terminal(usize, usize),
    SshHost(usize),
    Static,
}

struct HubRow {
    text: String,
    attrs: CellAttributes,
    kind: RowKind,
}

#[derive(Clone)]
enum HubAction {
    SwitchRuntime(String),
    ConnectSshHost(usize),
    WorktreeRepoChosen { repo: String, display: String },
    LaunchAgent(usize, String),
    PromptAddRuntime,
    PromptAddRepo,
    PromptWorktreeRepo,
    PromptCloneRepo,
    PromptCloneDestination { url: String },
    PromptNewRepo,
    PromptNewRepoName { parent: String },
}

use crate::picker::PickerRow;

type PickerEntry = crate::picker::PickerEntry<HubAction>;
type PickerGroup = crate::picker::PickerGroup<HubAction>;

enum SubmitKind {
    AddRuntime,
    AddRepo,
    WorktreeName { repo: String },
    CloneUrl,
    CloneDestination { url: String },
    NewRepoParent,
    NewRepoName { parent: String },
}

enum PickerStage {
    Browse,
    Input {
        label: String,
        hint: String,
        buffer: String,
        submit: SubmitKind,
        suggest: bool,
        suggestions: Vec<String>,
        suggestion_selected: Option<usize>,
        suggest_gen: u64,
    },
}

fn input_stage(label: &str, hint: &str, submit: SubmitKind, suggest: bool) -> PickerStage {
    PickerStage::Input {
        label: label.to_owned(),
        hint: hint.to_owned(),
        buffer: String::new(),
        submit,
        suggest,
        suggestions: Vec::new(),
        suggestion_selected: None,
        suggest_gen: 0,
    }
}

struct HubPicker {
    title: String,
    crumbs: Vec<String>,
    groups: Vec<PickerGroup>,
    selected: usize,
    stage: PickerStage,
}

#[derive(Clone)]
struct SshHostRow {
    id: String,
    label: String,
    status: String,
}

struct HubState {
    size: TerminalSize,
    seqno: SequenceNo,
    status: String,
    groups: Vec<WorktreeGroup>,
    agents: Vec<String>,
    rows: Vec<HubRow>,
    cursor: usize,
    scroll: usize,
    picker: Option<HubPicker>,
    ssh_hosts: Vec<SshHostRow>,
}

impl HubState {
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(picker) = &self.picker {
            if let PickerStage::Input {
                label,
                hint,
                buffer,
                suggestions,
                suggestion_selected,
                ..
            } = &picker.stage
            {
                rows.push(HubRow {
                    text: label.clone(),
                    attrs: attr_bold_fg(AnsiColor::Teal),
                    kind: RowKind::Static,
                });
                if !picker.crumbs.is_empty() {
                    rows.push(HubRow {
                        text: picker.crumbs.join("  \u{203a}  "),
                        attrs: attr_dim(),
                        kind: RowKind::Static,
                    });
                }
                rows.push(HubRow {
                    text: String::new(),
                    attrs: attr_default(),
                    kind: RowKind::Static,
                });
                for (index, suggestion) in suggestions.iter().enumerate() {
                    let active = *suggestion_selected == Some(index);
                    let marker = if active { "\u{276f} " } else { "  " };
                    rows.push(HubRow {
                        text: format!("{marker}{suggestion}"),
                        attrs: if active {
                            attr_bold_fg(AnsiColor::Teal)
                        } else {
                            attr_default()
                        },
                        kind: RowKind::Static,
                    });
                }
                if !suggestions.is_empty() {
                    rows.push(HubRow {
                        text: String::new(),
                        attrs: attr_default(),
                        kind: RowKind::Static,
                    });
                }
                let cols = self.size.cols.max(8);
                let shown = if buffer.chars().count() + 3 > cols {
                    let skip = buffer.chars().count() + 3 - cols;
                    buffer.chars().skip(skip).collect::<String>()
                } else {
                    buffer.clone()
                };
                rows.push(HubRow {
                    text: format!("\u{276f} {shown}\u{258f}"),
                    attrs: attr_default(),
                    kind: RowKind::Static,
                });
                rows.push(HubRow {
                    text: String::new(),
                    attrs: attr_default(),
                    kind: RowKind::Static,
                });
                rows.push(HubRow {
                    text: hint.clone(),
                    attrs: attr_dim(),
                    kind: RowKind::Static,
                });
                self.rows = rows;
                return;
            }

            let view = crate::picker::browse_view(
                &picker.title,
                &picker.crumbs,
                &picker.groups,
                picker.selected,
                self.size.cols.max(8),
            );
            for line in &view.lines {
                let (text, attrs) = line.flatten();
                rows.push(HubRow {
                    text,
                    attrs,
                    kind: RowKind::Static,
                });
            }
            rows.push(HubRow {
                text: String::new(),
                attrs: attr_default(),
                kind: RowKind::Static,
            });
            rows.push(HubRow {
                text: "enter select \u{b7} esc back".to_owned(),
                attrs: attr_dim(),
                kind: RowKind::Static,
            });
            self.rows = rows;
            return;
        }

        rows.push(HubRow {
            text: "Orca".to_owned(),
            attrs: attr_bold(),
            kind: RowKind::Static,
        });
        rows.push(HubRow {
            text: String::new(),
            attrs: attr_default(),
            kind: RowKind::Static,
        });
        for (gi, group) in self.groups.iter().enumerate() {
            let fold = if group.folded { "▸" } else { "▾" };
            let text = format!(
                "{fold} {} [{}] {} · {} terminals",
                group.display,
                group.branch,
                group.status,
                group.terminals.len()
            );
            rows.push(HubRow {
                text,
                attrs: attr_bold_fg(AnsiColor::Aqua),
                kind: RowKind::Group(gi),
            });
            if group.folded {
                continue;
            }
            for (ti, terminal) in group.terminals.iter().enumerate() {
                let glyph = match (&terminal.agent, terminal.connected) {
                    (_, false) => "○",
                    (Some(_), true) => "●",
                    (None, true) => " ",
                };
                let agent = terminal
                    .agent
                    .as_deref()
                    .map(|a| format!(" · {a}"))
                    .unwrap_or_default();
                rows.push(HubRow {
                    text: format!("  {glyph} {}{agent}", terminal.title),
                    attrs: attr_default(),
                    kind: RowKind::Terminal(gi, ti),
                });
            }
        }
        if self.groups.is_empty() {
            rows.push(HubRow {
                text: "no worktrees; add a repo in orca".to_owned(),
                attrs: attr_dim(),
                kind: RowKind::Static,
            });
        }
        if !self.ssh_hosts.is_empty() {
            rows.push(HubRow {
                text: String::new(),
                attrs: attr_default(),
                kind: RowKind::Static,
            });
            rows.push(HubRow {
                text: "SSH hosts".to_owned(),
                attrs: attr_bold(),
                kind: RowKind::Static,
            });
            for (index, host) in self.ssh_hosts.iter().enumerate() {
                let glyph = match host.status.as_str() {
                    "connected" => "●",
                    "connecting" | "deploying-relay" | "reconnecting" => "…",
                    "disconnected" => "○",
                    _ => "✗",
                };
                rows.push(HubRow {
                    text: format!("  {glyph} {} · {}", host.label, host.status),
                    attrs: attr_default(),
                    kind: RowKind::SshHost(index),
                });
            }
        }
        rows.push(HubRow {
            text: String::new(),
            attrs: attr_default(),
            kind: RowKind::Static,
        });
        rows.push(HubRow {
            text:
                "enter open · T split · t terminal · n agent · a repo · c worktree · p actions · o fold · + runtime · r refresh · q close"
                    .to_owned(),
            attrs: attr_dim(),
            kind: RowKind::Static,
        });
        self.rows = rows;
        self.clamp_cursor();
    }

    fn selectable(&self, index: usize) -> bool {
        matches!(
            self.rows.get(index).map(|row| row.kind),
            Some(RowKind::Group(_)) | Some(RowKind::Terminal(..)) | Some(RowKind::SshHost(_))
        )
    }

    fn clamp_cursor(&mut self) {
        if self.selectable(self.cursor) {
            return;
        }
        self.cursor = (0..self.rows.len())
            .find(|&index| self.selectable(index))
            .unwrap_or(0);
    }

    fn move_cursor(&mut self, delta: isize) {
        let mut index = self.cursor as isize;
        loop {
            index += delta;
            if index < 0 || index as usize >= self.rows.len() {
                return;
            }
            if self.selectable(index as usize) {
                self.cursor = index as usize;
                self.scroll_to_cursor();
                return;
            }
        }
    }

    fn scroll_to_cursor(&mut self) {
        let rows = self.size.rows.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + rows {
            self.scroll = self.cursor + 1 - rows;
        }
    }

    fn footer_row(&self) -> usize {
        self.size.rows.saturating_sub(1)
    }

    fn row_line(&self, screen_row: usize) -> Line {
        let cols = self.size.cols;
        if !self.status.is_empty() && screen_row == self.footer_row() {
            return make_line(&self.status, &attr_fg(AnsiColor::Yellow), self.seqno, cols);
        }
        let index = self.scroll + screen_row;
        let Some(row) = self.rows.get(index) else {
            return make_line("", &attr_default(), self.seqno, cols);
        };
        let mut line = make_line(&row.text, &row.attrs, self.seqno, cols);
        if index == self.cursor && self.picker.is_none() && self.selectable(index) {
            for cell in line.cells_mut_for_attr_changes_only() {
                cell.attrs_mut().set_reverse(true);
            }
        }
        line
    }
}

pub struct OrcaHubPane {
    pane_id: PaneId,
    domain: Mutex<Option<Arc<dyn Domain>>>,
    window: Window,
    mux_window_id: WindowId,
    me: Mutex<Weak<OrcaHubPane>>,
    watching: AtomicBool,
    disconnect_watching: AtomicBool,
    state: Mutex<HubState>,
}

impl OrcaHubPane {
    fn new(
        domain: Option<Arc<dyn Domain>>,
        window: Window,
        mux_window_id: WindowId,
        size: TerminalSize,
    ) -> Arc<OrcaHubPane> {
        let pane = Arc::new(OrcaHubPane {
            pane_id: alloc_pane_id(),
            domain: Mutex::new(domain),
            window,
            mux_window_id,
            me: Mutex::new(Weak::new()),
            watching: AtomicBool::new(false),
            disconnect_watching: AtomicBool::new(false),
            state: Mutex::new(HubState {
                size,
                seqno: 1,
                status: "connecting…".to_owned(),
                groups: Vec::new(),
                agents: Vec::new(),
                rows: Vec::new(),
                cursor: 0,
                scroll: 0,
                picker: None,
                ssh_hosts: Vec::new(),
            }),
        });
        *pane.me.lock() = Arc::downgrade(&pane);
        pane
    }

    fn arc(&self) -> Option<Arc<OrcaHubPane>> {
        self.me.lock().upgrade()
    }

    fn domain(&self) -> Option<Arc<dyn Domain>> {
        self.domain.lock().clone()
    }

    fn mutate<T, F: FnOnce(&mut HubState) -> T>(&self, f: F) -> T {
        let value = {
            let mut state = self.state.lock();
            let value = f(&mut state);
            state.rebuild_rows();
            state.seqno += 1;
            value
        };
        self.window.invalidate();
        value
    }

    fn set_status(&self, status: impl Into<String>) {
        self.mutate(|state| state.status = status.into());
    }

    fn start(self: &Arc<Self>) {
        self.spawn_refresh();
    }

    fn spawn_refresh(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            if let Err(err) = pane.refresh().await {
                pane.set_status(format!("orca: {err:#}"));
            }
        })
        .detach();
    }

    async fn refresh(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some(domain) = self.domain() else {
            self.set_status("no orca runtime; press + to pair one");
            return Ok(());
        };
        let orca = domain
            .downcast_ref::<OrcaDomain>()
            .ok_or_else(|| anyhow!("hub pane is not bound to an orca domain"))?;
        let client = orca.ensure_client().await?;

        let summaries = client.worktree_ps().await?;
        let terminals = client.list_terminals(None).await?;
        let agents = client.detect_agents().await.unwrap_or_default();

        let mut by_worktree: HashMap<String, Vec<TerminalRow>> = HashMap::new();
        for terminal in terminals {
            if terminal.pty_id.is_none() {
                continue;
            }
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
            by_worktree
                .entry(terminal.worktree_id.clone())
                .or_default()
                .push(TerminalRow {
                    parent_tab_id: terminal.tab_id.clone(),
                    leaf_id: terminal.leaf_id.clone(),
                    handle: terminal.handle,
                    worktree_selector: id_selector(&terminal.worktree_id),
                    worktree_path: terminal.worktree_path,
                    title,
                    agent: terminal.agent_identity,
                    connected: terminal.connected,
                });
        }

        let mut groups = Vec::new();
        for summary in summaries {
            if summary.is_archived {
                continue;
            }
            let terminals = by_worktree.remove(&summary.worktree_id).unwrap_or_default();
            let branch = summary
                .branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&summary.branch)
                .to_owned();
            let display = if summary.display_name.is_empty() || summary.display_name == branch {
                std::path::Path::new(&summary.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or(summary.display_name)
            } else {
                summary.display_name
            };
            groups.push(WorktreeGroup {
                selector: id_selector(&summary.worktree_id),
                cwd: summary.path,
                display,
                branch,
                status: summary.status,
                folded: false,
                terminals,
            });
        }

        let mut ssh_hosts = Vec::new();
        for target in client.list_ssh_targets().await.unwrap_or_default() {
            let status = client
                .ssh_target_state(&target.id)
                .await
                .ok()
                .flatten()
                .map(|state| state.status)
                .filter(|status| !status.is_empty())
                .unwrap_or_else(|| "disconnected".to_owned());
            ssh_hosts.push(SshHostRow {
                id: target.id,
                label: target.label,
                status,
            });
        }

        self.mutate(|state| {
            state.status = String::new();
            state.groups = groups;
            state.agents = agents;
            state.ssh_hosts = ssh_hosts;
        });
        self.spawn_watch(client.clone());
        self.spawn_tabs_watch(client);
        Ok(())
    }

    fn spawn_tabs_watch(self: &Arc<Self>, client: OrcaClient) {
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let events = match client.subscribe_session_tabs_all().await {
                Ok(events) => events,
                Err(_) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.watching.store(false, Ordering::SeqCst);
                    }
                    return;
                }
            };
            while events.recv_async().await.is_ok() {
                smol::Timer::after(Duration::from_millis(300)).await;
                while events.try_recv().is_ok() {}
                let Some(pane) = weak.upgrade() else {
                    return;
                };
                if let Err(err) = pane.refresh().await {
                    pane.set_status(format!("orca: {err:#}"));
                }
            }
            if let Some(pane) = weak.upgrade() {
                pane.watching.store(false, Ordering::SeqCst);
            }
        })
        .detach();
    }

    fn spawn_watch(self: &Arc<Self>, client: OrcaClient) {
        if self.disconnect_watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        let mut events = client.events();
        promise::spawn::spawn(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    RuntimeEvent::Disconnected => break,
                }
            }
            let Some(pane) = weak.upgrade() else {
                return;
            };
            pane.disconnect_watching.store(false, Ordering::SeqCst);
            pane.spawn_reconnect();
        })
        .detach();
    }

    fn spawn_reconnect(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let mut delay = RECONNECT_MIN_DELAY;
            loop {
                let Some(pane) = weak.upgrade() else {
                    return;
                };
                pane.set_status("orca disconnected; reconnecting…");
                let Some(domain) = pane.domain() else {
                    return;
                };
                let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                    return;
                };
                orca.reset_client();
                match orca.ensure_client().await {
                    Ok(_) => {
                        pane.spawn_refresh();
                        return;
                    }
                    Err(_) => {
                        drop(pane);
                        smol::Timer::after(delay).await;
                        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
                    }
                }
            }
        })
        .detach();
    }

    fn selected(&self) -> Option<(RowKind, WorktreeGroup)> {
        let state = self.state.lock();
        let kind = state.rows.get(state.cursor)?.kind;
        let group = match kind {
            RowKind::Group(gi) | RowKind::Terminal(gi, _) => state.groups.get(gi)?.clone(),
            RowKind::SshHost(_) | RowKind::Static => return None,
        };
        Some((kind, group))
    }

    fn open_selected(self: &Arc<Self>, split: bool) {
        {
            let state = self.state.lock();
            if let Some(RowKind::SshHost(index)) = state.rows.get(state.cursor).map(|row| row.kind)
            {
                let host = state.ssh_hosts.get(index).cloned();
                drop(state);
                if let Some(host) = host {
                    self.connect_ssh_host(host);
                }
                return;
            }
        }
        let Some((kind, group)) = self.selected() else {
            return;
        };
        match kind {
            RowKind::Terminal(_, ti) => {
                let Some(terminal) = group.terminals.get(ti).cloned() else {
                    return;
                };
                self.open_terminal(terminal, split);
            }
            RowKind::Group(_) => self.new_terminal(group, split),
            RowKind::SshHost(_) | RowKind::Static => {}
        }
    }

    fn open_terminal(self: &Arc<Self>, terminal: TerminalRow, split: bool) {
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            if let Err(err) = pane.open_terminal_impl(terminal, split).await {
                pane.set_status(format!("orca: {err:#}"));
            }
        })
        .detach();
    }

    async fn open_terminal_impl(
        self: &Arc<Self>,
        terminal: TerminalRow,
        split: bool,
    ) -> anyhow::Result<()> {
        let domain = self
            .domain()
            .ok_or_else(|| anyhow!("no orca runtime; press + to pair one"))?;
        let orca = domain
            .downcast_ref::<OrcaDomain>()
            .ok_or_else(|| anyhow!("hub pane is not bound to an orca domain"))?;
        let client = orca.ensure_client().await?;
        let size = self.state.lock().size;
        let mux = Mux::get();

        if split {
            let (_, _, tab_id) = mux
                .resolve_pane_id(self.pane_id)
                .ok_or_else(|| anyhow!("hub pane is not in a tab"))?;
            let tab = mux
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("hub tab is gone"))?;
            let pane_index = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .find(|p| p.pane.pane_id() == self.pane_id)
                .map(|p| p.index)
                .ok_or_else(|| anyhow!("hub pane not in tab"))?;
            let request = SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: MuxSplitSize::Percent(50),
                top_level: false,
            };
            let split_size = tab
                .compute_split_size(pane_index, request)
                .ok_or_else(|| anyhow!("cannot compute split size"))?;
            let pane = orca
                .attach_terminal(
                    &client,
                    TerminalBinding {
                        terminal: terminal.handle,
                        worktree_selector: terminal.worktree_selector,
                        worktree_path: terminal.worktree_path,
                        parent_tab_id: terminal.parent_tab_id,
                        leaf_id: terminal.leaf_id,
                    },
                    split_size.second,
                )
                .await?;
            tab.split_and_insert(pane_index, request, pane as Arc<dyn Pane>)?;
        } else {
            let pane = orca
                .attach_terminal(
                    &client,
                    TerminalBinding {
                        terminal: terminal.handle,
                        worktree_selector: terminal.worktree_selector,
                        worktree_path: terminal.worktree_path,
                        parent_tab_id: terminal.parent_tab_id,
                        leaf_id: terminal.leaf_id,
                    },
                    size,
                )
                .await?;
            let tab = Arc::new(MuxTab::new(&size));
            tab.assign_pane(&(pane as Arc<dyn Pane>));
            mux.add_tab_and_active_pane(&tab)?;
            mux.add_tab_to_window(&tab, self.mux_window_id)?;
        }
        Ok(())
    }

    fn new_terminal(self: &Arc<Self>, group: WorktreeGroup, split: bool) {
        let Some(domain) = self.domain() else {
            self.set_status("no orca runtime; press + to pair one");
            return;
        };
        let size = self.state.lock().size;
        let pane_id = self.pane_id;
        let window_id = self.mux_window_id;
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let result = if split {
                match Mux::get().resolve_pane_id(pane_id) {
                    Some((_, _, tab_id)) => domain
                        .split_pane(
                            SplitSource::Spawn {
                                command: None,
                                command_dir: Some(group.cwd.clone()),
                            },
                            tab_id,
                            pane_id,
                            SplitRequest::default(),
                        )
                        .await
                        .map(|_| ()),
                    None => Err(anyhow!("hub pane is not in a tab")),
                }
            } else {
                domain
                    .spawn(size, None, Some(group.cwd.clone()), window_id)
                    .await
                    .map(|_| ())
            };
            if let Err(err) = result {
                if let Some(pane) = weak.upgrade() {
                    pane.set_status(format!("orca: {err:#}"));
                }
            }
        })
        .detach();
    }

    fn connect_ssh_host(self: &Arc<Self>, host: SshHostRow) {
        let Some(domain) = self.domain() else {
            return;
        };
        let weak = Arc::downgrade(self);
        self.set_status(format!("connecting {}…", host.label));
        promise::spawn::spawn(async move {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                return;
            };
            let client = match orca.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    pane.set_status(format!("orca: {err:#}"));
                    return;
                }
            };
            match client.connect_ssh_target(&host.id).await {
                Ok(state) => {
                    let status = state
                        .map(|state| state.status)
                        .filter(|status| !status.is_empty())
                        .unwrap_or_else(|| "connected".to_owned());
                    pane.set_status(format!("{}: {status}", host.label));
                }
                Err(err) => pane.set_status(format!("{}: {err}", host.label)),
            }
            if let Err(err) = pane.refresh().await {
                pane.set_status(format!("orca: {err:#}"));
            }
        })
        .detach();
    }

    fn with_client<F>(self: &Arc<Self>, action: F)
    where
        F: FnOnce(Arc<OrcaHubPane>, OrcaClient) -> futures::future::BoxFuture<'static, ()>
            + 'static,
    {
        let Some(domain) = self.domain() else {
            self.set_status("no orca runtime; press + to pair one");
            return;
        };
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                return;
            };
            let client = match orca.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    pane.set_status(format!("orca: {err:#}"));
                    return;
                }
            };
            action(pane, client).await;
        })
        .detach();
    }

    fn run_add_repo(self: &Arc<Self>, value: String) {
        if value.is_empty() {
            self.set_status("add repo cancelled");
            return;
        }
        self.set_status(format!("adding repo {value}…"));
        self.with_client(move |pane, client| {
            Box::pin(async move {
                match client.add_repo(&value).await {
                    Ok(()) => {
                        pane.set_status(format!("added repo {value}; create a worktree with c"));
                    }
                    Err(err) => pane.set_status(format!("repo add: {err}")),
                }
                let _ = pane.refresh().await;
            })
        });
    }

    fn apply_picker_action(self: &Arc<Self>, action: HubAction) {
        match action {
            HubAction::SwitchRuntime(name) => {
                self.mutate(|state| state.picker = None);
                let Some(domain) = Mux::get().get_domain_by_name(&name) else {
                    self.set_status(format!("{name} is gone"));
                    return;
                };
                *self.domain.lock() = Some(domain);
                self.watching.store(false, Ordering::SeqCst);
                self.set_status(format!("switched to {name}"));
                self.spawn_refresh();
            }
            HubAction::ConnectSshHost(index) => {
                let host = self.mutate(|state| {
                    state.picker = None;
                    state.ssh_hosts.get(index).cloned()
                });
                if let Some(host) = host {
                    self.connect_ssh_host(host);
                }
            }
            HubAction::LaunchAgent(group, agent) => {
                self.mutate(|state| state.picker = None);
                self.launch_agent(group, agent);
            }
            HubAction::PromptAddRuntime => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: Vec::new(),
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Add runtime",
                        "paste an orca://pair?\u{2026} URL or ssh://host \u{b7} enter connect \u{b7} esc back",
                        SubmitKind::AddRuntime,
                        false,
                    ),
                });
            }),
            HubAction::PromptAddRepo => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: Vec::new(),
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Add repo \u{2014} path on the runtime host",
                        "type to browse \u{b7} \u{2191}/\u{2193} pick \u{b7} tab descend \u{b7} enter add \u{b7} esc back",
                        SubmitKind::AddRepo,
                        true,
                    ),
                });
            }),
            HubAction::PromptWorktreeRepo => self.mutate(|state| {
                let entries = state
                    .groups
                    .iter()
                    .map(|group| PickerEntry {
                        dot: None,
                        indent: false,
                        label: group.display.clone(),
                        detail: Some(group.cwd.clone()),
                        action: HubAction::WorktreeRepoChosen {
                            repo: group.cwd.clone(),
                            display: group.display.clone(),
                        },
                    })
                    .collect::<Vec<_>>();
                if entries.is_empty() {
                    state.status = "no repos yet; add one with a".to_owned();
                    return;
                }
                state.picker = Some(HubPicker {
                    title: "Create worktree".to_owned(),
                    crumbs: Vec::new(),
                    groups: vec![PickerGroup {
                        label: "Repos".to_owned(),
                        collapsed: false,
                        entries,
                    }],
                    selected: 1,
                    stage: PickerStage::Browse,
                });
            }),
            HubAction::PromptCloneRepo => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: vec!["clone git repo".to_owned()],
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Clone URL",
                        "git/https URL \u{b7} enter next \u{b7} esc back",
                        SubmitKind::CloneUrl,
                        false,
                    ),
                });
            }),
            HubAction::PromptCloneDestination { url } => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: vec!["clone git repo".to_owned(), url.clone()],
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Destination directory on the runtime host",
                        "type to browse \u{b7} \u{2191}/\u{2193} pick \u{b7} tab descend \u{b7} enter clone \u{b7} esc back",
                        SubmitKind::CloneDestination { url },
                        true,
                    ),
                });
            }),
            HubAction::PromptNewRepo => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: vec!["new repo".to_owned()],
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Parent directory on the runtime host",
                        "type to browse \u{b7} \u{2191}/\u{2193} pick \u{b7} tab descend \u{b7} enter next \u{b7} esc back",
                        SubmitKind::NewRepoParent,
                        true,
                    ),
                });
            }),
            HubAction::PromptNewRepoName { parent } => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: vec!["new repo".to_owned(), parent.clone()],
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Repo name",
                        "enter create \u{b7} esc back",
                        SubmitKind::NewRepoName { parent },
                        false,
                    ),
                });
            }),
            HubAction::WorktreeRepoChosen { repo, display } => self.mutate(|state| {
                state.picker = Some(HubPicker {
                    title: String::new(),
                    crumbs: vec!["create worktree".to_owned(), display],
                    groups: Vec::new(),
                    selected: 0,
                    stage: input_stage(
                        "Worktree name",
                        "enter create \u{b7} esc back",
                        SubmitKind::WorktreeName { repo },
                        false,
                    ),
                });
            }),
        }
    }

    fn refetch_suggestions(self: &Arc<Self>) {
        let request = self.mutate(|state| {
            if let Some(HubPicker {
                stage:
                    PickerStage::Input {
                        buffer,
                        suggest,
                        suggest_gen,
                        suggestion_selected,
                        ..
                    },
                ..
            }) = &mut state.picker
            {
                if !*suggest {
                    return None;
                }
                *suggest_gen += 1;
                *suggestion_selected = None;
                return Some((buffer.clone(), *suggest_gen));
            }
            None
        });
        let Some((buffer, generation)) = request else {
            return;
        };
        let Some(domain) = self.domain() else {
            return;
        };
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(150)).await;
            let Some(pane) = weak.upgrade() else {
                return;
            };
            let current = pane.mutate(|state| {
                if let Some(HubPicker {
                    stage: PickerStage::Input { suggest_gen, .. },
                    ..
                }) = &state.picker
                {
                    *suggest_gen
                } else {
                    0
                }
            });
            if current != generation {
                return;
            }
            let Some(orca) = domain.downcast_ref::<OrcaDomain>() else {
                return;
            };
            let Ok(client) = orca.ensure_client().await else {
                return;
            };
            let (dir, prefix) = match buffer.rsplit_once('/') {
                Some((dir, prefix)) => (
                    if dir.is_empty() { "/" } else { dir }.to_owned(),
                    prefix.to_owned(),
                ),
                None => (String::new(), buffer.clone()),
            };
            let Ok(listing) = client.browse_server_dir(&dir).await else {
                return;
            };
            let prefix_lower = prefix.to_lowercase();
            let base = listing.resolved_path.trim_end_matches('/').to_owned();
            let suggestions = listing
                .entries
                .iter()
                .filter(|entry| entry.is_directory)
                .filter(|entry| {
                    (prefix.starts_with('.') || !entry.name.starts_with('.'))
                        && entry.name.to_lowercase().starts_with(&prefix_lower)
                })
                .take(8)
                .map(|entry| format!("{base}/{}", entry.name))
                .collect::<Vec<_>>();
            let Some(pane) = weak.upgrade() else {
                return;
            };
            pane.mutate(|state| {
                if let Some(HubPicker {
                    stage:
                        PickerStage::Input {
                            suggestions: slot,
                            suggest_gen,
                            ..
                        },
                    ..
                }) = &mut state.picker
                {
                    if *suggest_gen == generation {
                        *slot = suggestions;
                    }
                }
            });
        })
        .detach();
    }

    fn runtime_entries(&self) -> Vec<PickerEntry> {
        let current = self
            .domain()
            .map(|domain| domain.domain_name().to_owned())
            .unwrap_or_default();
        Mux::get()
            .iter_domains()
            .into_iter()
            .filter(|domain| domain.downcast_ref::<OrcaDomain>().is_some())
            .map(|domain| {
                let name = domain.domain_name().to_owned();
                let attached = matches!(domain.state(), mux::domain::DomainState::Attached);
                let dot = if attached {
                    Some(("\u{25cf}", AnsiColor::Green))
                } else {
                    Some(("\u{25cb}", AnsiColor::Grey))
                };
                let detail = if name == current {
                    Some("current".to_owned())
                } else if attached {
                    Some("connected".to_owned())
                } else {
                    None
                };
                PickerEntry {
                    dot,
                    indent: false,
                    label: name.clone(),
                    detail,
                    action: HubAction::SwitchRuntime(name),
                }
            })
            .collect()
    }

    fn open_runtimes_picker(self: &Arc<Self>) {
        let runtimes = self.runtime_entries();
        self.mutate(|state| {
            let mut groups = Vec::new();
            if !runtimes.is_empty() {
                groups.push(PickerGroup {
                    label: "Runtimes".to_owned(),
                    collapsed: false,
                    entries: runtimes,
                });
            }
            groups.push(PickerGroup {
                label: "New".to_owned(),
                collapsed: false,
                entries: vec![PickerEntry::plain(
                    "add runtime  (pairing URL or ssh://host)",
                    HubAction::PromptAddRuntime,
                )],
            });
            state.picker = Some(HubPicker {
                title: "Orca \u{2014} runtimes".to_owned(),
                crumbs: Vec::new(),
                groups,
                selected: 1,
                stage: PickerStage::Browse,
            });
        });
    }

    fn open_actions_picker(self: &Arc<Self>) {
        let runtimes = self.runtime_entries();
        self.mutate(|state| {
            let hosts = state
                .ssh_hosts
                .iter()
                .enumerate()
                .map(|(index, host)| PickerEntry {
                    dot: None,
                    indent: false,
                    label: host.label.clone(),
                    detail: Some(host.status.clone()),
                    action: HubAction::ConnectSshHost(index),
                })
                .collect::<Vec<_>>();
            let mut groups = Vec::new();
            if !runtimes.is_empty() {
                groups.push(PickerGroup {
                    label: "Runtimes".to_owned(),
                    collapsed: false,
                    entries: runtimes.clone(),
                });
            }
            if !hosts.is_empty() {
                groups.push(PickerGroup {
                    label: "SSH hosts".to_owned(),
                    collapsed: false,
                    entries: hosts,
                });
            }
            groups.push(PickerGroup {
                label: "New".to_owned(),
                collapsed: false,
                entries: vec![
                    PickerEntry::plain(
                        "add runtime  (pairing URL or ssh://host)",
                        HubAction::PromptAddRuntime,
                    ),
                    PickerEntry::plain("add repo  (path on host)", HubAction::PromptAddRepo),
                    PickerEntry::plain("clone git repo", HubAction::PromptCloneRepo),
                    PickerEntry::plain("new repo", HubAction::PromptNewRepo),
                    PickerEntry::plain("create worktree", HubAction::PromptWorktreeRepo),
                ],
            });
            state.picker = Some(HubPicker {
                title: "Orca \u{2014} actions".to_owned(),
                crumbs: Vec::new(),
                groups,
                selected: 1,
                stage: PickerStage::Browse,
            });
        });
    }

    fn run_create_worktree(self: &Arc<Self>, repo: String, name: String) {
        if repo.is_empty() || name.is_empty() {
            self.set_status("create worktree cancelled");
            return;
        }
        let selector = if repo.contains(':') {
            repo.clone()
        } else {
            format!("path:{repo}")
        };
        self.set_status(format!("creating worktree {name}…"));
        self.with_client(move |pane, client| {
            Box::pin(async move {
                match client.create_worktree(&selector, &name).await {
                    Ok(()) => pane.set_status(format!("created worktree {name}")),
                    Err(err) => pane.set_status(format!("worktree create: {err}")),
                }
                let _ = pane.refresh().await;
            })
        });
    }

    fn run_clone_repo(self: &Arc<Self>, url: String, destination: String) {
        if destination.is_empty() {
            self.set_status("clone cancelled");
            return;
        }
        self.set_status(format!("cloning {url}\u{2026}"));
        self.with_client(move |pane, client| {
            Box::pin(async move {
                match client.clone_repo(&url, &destination).await {
                    Ok(()) => pane.set_status(format!("cloned into {destination}")),
                    Err(err) => pane.set_status(format!("clone: {err}")),
                }
                let _ = pane.refresh().await;
            })
        });
    }

    fn run_create_repo(self: &Arc<Self>, parent: String, name: String) {
        if name.is_empty() {
            self.set_status("new repo cancelled");
            return;
        }
        self.set_status(format!("creating {name}\u{2026}"));
        self.with_client(move |pane, client| {
            Box::pin(async move {
                match client.create_repo(&parent, &name).await {
                    Ok(()) => pane.set_status(format!("created {parent}/{name}")),
                    Err(err) => pane.set_status(format!("new repo: {err}")),
                }
                let _ = pane.refresh().await;
            })
        });
    }

    fn run_add_runtime(self: &Arc<Self>, value: String) {
        if value.is_empty() {
            self.set_status("add runtime cancelled");
            return;
        }
        let (name, build): (String, Box<dyn FnOnce(String) -> OrcaDomain>) =
            if let Some(target) = value.strip_prefix("ssh://") {
                let target = target.trim_end_matches('/').to_owned();
                if target.is_empty() {
                    self.set_status("ssh runtime needs a host, e.g. ssh://user@host");
                    return;
                }
                (
                    format!("orca:{target}"),
                    Box::new(move |name| OrcaDomain::new_ssh(name, target)),
                )
            } else {
                let offer = match PairingOffer::parse(&value) {
                    Ok(offer) => offer,
                    Err(err) => {
                        self.set_status(format!("invalid pairing URL: {err}"));
                        return;
                    }
                };
                let host = offer
                    .endpoint()
                    .trim_start_matches("wss://")
                    .trim_start_matches("ws://")
                    .trim_end_matches('/')
                    .to_owned();
                (
                    format!("orca:{host}"),
                    Box::new(move |name| OrcaDomain::new(name, offer)),
                )
            };
        let mux = Mux::get();
        let domain = match mux.get_domain_by_name(&name) {
            Some(domain) => {
                self.set_status(format!("{name} already added; switching"));
                domain
            }
            None => {
                let domain: Arc<dyn Domain> = Arc::new(build(name.clone()));
                mux.add_domain(&domain);
                self.set_status(format!("added {name}; connecting…"));
                domain
            }
        };
        *self.domain.lock() = Some(domain);
        self.watching.store(false, Ordering::SeqCst);
        self.spawn_refresh();
    }

    fn launch_agent(self: &Arc<Self>, group_index: usize, agent: String) {
        let weak = Arc::downgrade(self);
        promise::spawn::spawn(async move {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            if let Err(err) = pane.launch_agent_impl(group_index, agent).await {
                pane.set_status(format!("orca: {err:#}"));
            }
        })
        .detach();
    }

    async fn launch_agent_impl(
        self: &Arc<Self>,
        group_index: usize,
        agent: String,
    ) -> anyhow::Result<()> {
        let domain = self
            .domain()
            .ok_or_else(|| anyhow!("no orca runtime; press + to pair one"))?;
        let orca = domain
            .downcast_ref::<OrcaDomain>()
            .ok_or_else(|| anyhow!("hub pane is not bound to an orca domain"))?;
        let client = orca.ensure_client().await?;
        let (selector, worktree_path) = self
            .state
            .lock()
            .groups
            .get(group_index)
            .map(|group| (group.selector.clone(), group.cwd.clone()))
            .ok_or_else(|| anyhow!("worktree is gone"))?;
        let size = self.state.lock().size;

        let tab = client
            .create_session_terminal(&CreateTerminalOpts {
                worktree: selector.clone(),
                launch_agent: Some(agent),
                ..CreateTerminalOpts::default()
            })
            .await?;
        let terminal = tab
            .terminal
            .clone()
            .ok_or_else(|| anyhow!("orca terminal is still provisioning; refresh and open it"))?;

        let pane = orca
            .attach_terminal(
                &client,
                TerminalBinding {
                    terminal,
                    worktree_selector: selector,
                    worktree_path,
                    parent_tab_id: tab.parent_tab_id.clone(),
                    leaf_id: tab.leaf_id.clone(),
                },
                size,
            )
            .await?;
        let mux = Mux::get();
        let mux_tab = Arc::new(MuxTab::new(&size));
        mux_tab.assign_pane(&(pane as Arc<dyn Pane>));
        mux.add_tab_and_active_pane(&mux_tab)?;
        mux.add_tab_to_window(&mux_tab, self.mux_window_id)?;
        self.spawn_refresh();
        Ok(())
    }

    fn close_self(&self) {
        let pane_id = self.pane_id;
        promise::spawn::spawn(async move {
            let mux = Mux::get();
            mux.remove_pane(pane_id);
            mux.prune_dead_windows();
        })
        .detach();
    }
}

impl Pane for OrcaHubPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn domain_id(&self) -> DomainId {
        self.domain
            .lock()
            .as_ref()
            .map(|domain| domain.domain_id())
            .unwrap_or(0)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.state.lock().seqno
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let state = self.state.lock();
        let mut set = RangeSet::new();
        if state.seqno > seqno {
            for row in lines.start.max(0)..lines.end {
                set.add(row);
            }
        }
        set
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        StableCursorPosition {
            x: 0,
            y: 0,
            shape: termwiz::surface::CursorShape::Default,
            visibility: CursorVisibility::Hidden,
        }
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let state = self.state.lock();
        let start = lines.start.max(0);
        let mut out = Vec::new();
        for index in start..lines.end.max(start) {
            out.push(state.row_line(index as usize));
        }
        (start, out)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let state = self.state.lock();
        let start = lines.start.max(0);
        let mut built = (start..lines.end.max(start))
            .map(|index| state.row_line(index as usize))
            .collect::<Vec<_>>();
        let mut refs = built.iter_mut().collect::<Vec<_>>();
        with_lines.with_lines_mut(start, &mut refs);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        let state = self.state.lock();
        RenderableDimensions {
            cols: state.size.cols,
            viewport_rows: state.size.rows,
            scrollback_rows: state.size.rows,
            physical_top: 0,
            scrollback_top: 0,
            dpi: state.size.dpi,
            pixel_width: state.size.pixel_width,
            pixel_height: state.size.pixel_height,
            reverse_video: false,
        }
    }

    fn get_title(&self) -> String {
        "Orca".to_owned()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        self.mutate(|state| {
            if let Some(HubPicker {
                stage: PickerStage::Input { buffer, .. },
                ..
            }) = &mut state.picker
            {
                buffer.push_str(text.trim());
            }
        });
        if let Some(this) = self.arc() {
            this.refetch_suggestions();
        }
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        static SINK: Mutex<Sink> = Mutex::new(Sink);
        MutexGuard::map(SINK.lock(), |sink| {
            let w: &mut dyn std::io::Write = sink;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.mutate(|state| state.size = size);
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let Some(this) = self.arc() else {
            return Ok(());
        };

        let picker_stage = {
            let state = self.state.lock();
            state.picker.as_ref().map(|picker| {
                (
                    matches!(picker.stage, PickerStage::Input { .. }),
                    picker.groups.is_empty(),
                )
            })
        };
        if let Some((in_input, no_groups)) = picker_stage {
            if in_input {
                match (key, mods) {
                    (KeyCode::Enter, _) => {
                        let submitted = self.mutate(|state| {
                            let picker = state.picker.as_mut()?;
                            let stage = std::mem::replace(&mut picker.stage, PickerStage::Browse);
                            let PickerStage::Input {
                                buffer,
                                submit,
                                suggestions,
                                suggestion_selected,
                                ..
                            } = stage
                            else {
                                return None;
                            };
                            state.picker = None;
                            let value = suggestion_selected
                                .and_then(|index| suggestions.get(index).cloned())
                                .unwrap_or(buffer);
                            Some((submit, value.trim().to_owned()))
                        });
                        if let Some((submit, value)) = submitted {
                            match submit {
                                SubmitKind::AddRuntime => this.run_add_runtime(value),
                                SubmitKind::AddRepo => this.run_add_repo(value),
                                SubmitKind::WorktreeName { repo } => {
                                    this.run_create_worktree(repo, value)
                                }
                                SubmitKind::CloneUrl => {
                                    if value.is_empty() {
                                        this.set_status("clone cancelled");
                                    } else {
                                        this.apply_picker_action(
                                            HubAction::PromptCloneDestination { url: value },
                                        );
                                    }
                                }
                                SubmitKind::CloneDestination { url } => {
                                    this.run_clone_repo(url, value)
                                }
                                SubmitKind::NewRepoParent => {
                                    if value.is_empty() {
                                        this.set_status("new repo cancelled");
                                    } else {
                                        this.apply_picker_action(HubAction::PromptNewRepoName {
                                            parent: value,
                                        });
                                    }
                                }
                                SubmitKind::NewRepoName { parent } => {
                                    this.run_create_repo(parent, value)
                                }
                            }
                        }
                    }
                    (KeyCode::Escape, _) => self.mutate(|state| {
                        if no_groups {
                            state.picker = None;
                        } else if let Some(picker) = &mut state.picker {
                            picker.stage = PickerStage::Browse;
                        }
                    }),
                    (KeyCode::UpArrow, _) | (KeyCode::Char('p'), KeyModifiers::CTRL) => {
                        self.mutate(|state| {
                            if let Some(HubPicker {
                                stage:
                                    PickerStage::Input {
                                        suggestions,
                                        suggestion_selected,
                                        ..
                                    },
                                ..
                            }) = &mut state.picker
                            {
                                let len = suggestions.len();
                                if len > 0 {
                                    *suggestion_selected = Some(match suggestion_selected {
                                        Some(0) | None => len - 1,
                                        Some(index) => *index - 1,
                                    });
                                }
                            }
                        });
                    }
                    (KeyCode::DownArrow, _) | (KeyCode::Char('n'), KeyModifiers::CTRL) => {
                        self.mutate(|state| {
                            if let Some(HubPicker {
                                stage:
                                    PickerStage::Input {
                                        suggestions,
                                        suggestion_selected,
                                        ..
                                    },
                                ..
                            }) = &mut state.picker
                            {
                                let len = suggestions.len();
                                if len > 0 {
                                    *suggestion_selected = Some(match suggestion_selected {
                                        None => 0,
                                        Some(index) => (*index + 1) % len,
                                    });
                                }
                            }
                        });
                    }
                    (KeyCode::Tab, _) => {
                        let changed = self.mutate(|state| {
                            if let Some(HubPicker {
                                stage:
                                    PickerStage::Input {
                                        buffer,
                                        suggestions,
                                        suggestion_selected,
                                        ..
                                    },
                                ..
                            }) = &mut state.picker
                            {
                                let pick = suggestion_selected
                                    .and_then(|index| suggestions.get(index))
                                    .or_else(|| suggestions.first());
                                if let Some(pick) = pick {
                                    *buffer = format!("{pick}/");
                                    *suggestion_selected = None;
                                    return true;
                                }
                            }
                            false
                        });
                        if changed {
                            this.refetch_suggestions();
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        self.mutate(|state| {
                            if let Some(HubPicker {
                                stage: PickerStage::Input { buffer, .. },
                                ..
                            }) = &mut state.picker
                            {
                                buffer.pop();
                            }
                        });
                        this.refetch_suggestions();
                    }
                    (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                        self.mutate(|state| {
                            if let Some(HubPicker {
                                stage: PickerStage::Input { buffer, .. },
                                ..
                            }) = &mut state.picker
                            {
                                buffer.clear();
                            }
                        });
                        this.refetch_suggestions();
                    }
                    (KeyCode::Char(c), mods)
                        if !mods.intersects(
                            KeyModifiers::CTRL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) && !c.is_control() =>
                    {
                        self.mutate(|state| {
                            if let Some(HubPicker {
                                stage: PickerStage::Input { buffer, .. },
                                ..
                            }) = &mut state.picker
                            {
                                buffer.push(c);
                            }
                        });
                        this.refetch_suggestions();
                    }
                    _ => {}
                }
                return Ok(());
            }
            match key {
                KeyCode::Char('j') | KeyCode::DownArrow => self.mutate(|state| {
                    if let Some(picker) = &mut state.picker {
                        let len = crate::picker::visible_rows(&picker.groups).len();
                        if len > 0 {
                            picker.selected = (picker.selected + 1) % len;
                        }
                    }
                }),
                KeyCode::Char('k') | KeyCode::UpArrow => self.mutate(|state| {
                    if let Some(picker) = &mut state.picker {
                        let len = crate::picker::visible_rows(&picker.groups).len();
                        if len > 0 {
                            picker.selected = (picker.selected + len - 1) % len;
                        }
                    }
                }),
                KeyCode::Enter => {
                    enum Chosen {
                        Toggle(usize),
                        Act(HubAction),
                    }
                    let chosen =
                        {
                            let state = self.state.lock();
                            state.picker.as_ref().and_then(|picker| {
                                match crate::picker::visible_rows(&picker.groups)
                                    .get(picker.selected)?
                                {
                                    PickerRow::Header(gi) => Some(Chosen::Toggle(*gi)),
                                    PickerRow::Entry(gi, ei) => Some(Chosen::Act(
                                        picker.groups.get(*gi)?.entries.get(*ei)?.action.clone(),
                                    )),
                                }
                            })
                        };
                    match chosen {
                        Some(Chosen::Toggle(gi)) => self.mutate(|state| {
                            if let Some(picker) = &mut state.picker {
                                if let Some(group) = picker.groups.get_mut(gi) {
                                    group.collapsed = !group.collapsed;
                                }
                                let len = crate::picker::visible_rows(&picker.groups).len();
                                if picker.selected >= len && len > 0 {
                                    picker.selected = len - 1;
                                }
                            }
                        }),
                        Some(Chosen::Act(action)) => this.apply_picker_action(action),
                        None => {}
                    }
                }
                KeyCode::Escape => self.mutate(|state| state.picker = None),
                _ => {}
            }
            return Ok(());
        }

        match (key, mods) {
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::DownArrow, _) => {
                self.mutate(|state| state.move_cursor(1));
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::UpArrow, _) => {
                self.mutate(|state| state.move_cursor(-1));
            }
            (KeyCode::Char('d'), KeyModifiers::CTRL) => {
                self.mutate(|state| {
                    let jump = (state.size.rows / 2).max(1) as isize;
                    state.move_cursor(jump);
                });
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                self.mutate(|state| {
                    let jump = (state.size.rows / 2).max(1) as isize;
                    state.move_cursor(-jump);
                });
            }
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.mutate(|state| {
                    state.cursor = 0;
                    state.scroll = 0;
                    state.clamp_cursor();
                });
            }
            (KeyCode::Char('G'), _) => {
                self.mutate(|state| {
                    state.cursor = state.rows.len().saturating_sub(1);
                    if !state.selectable(state.cursor) {
                        state.move_cursor(-1);
                    }
                    state.scroll_to_cursor();
                });
            }
            (KeyCode::Char('o'), KeyModifiers::NONE) => {
                self.mutate(|state| {
                    if let Some(RowKind::Group(gi) | RowKind::Terminal(gi, _)) =
                        state.rows.get(state.cursor).map(|row| row.kind)
                    {
                        if let Some(group) = state.groups.get_mut(gi) {
                            group.folded = !group.folded;
                        }
                    }
                });
            }
            (KeyCode::Enter, _) => this.open_selected(false),
            (KeyCode::Char('T'), _) => this.open_selected(true),
            (KeyCode::Char('t'), KeyModifiers::NONE) => {
                if let Some((_, group)) = self.selected() {
                    this.new_terminal(group, false);
                }
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) => {
                self.mutate(|state| {
                    if let Some(RowKind::Group(gi) | RowKind::Terminal(gi, _)) =
                        state.rows.get(state.cursor).map(|row| row.kind)
                    {
                        if state.agents.is_empty() {
                            state.status = "no agents detected on the orca host".to_owned();
                            return;
                        }
                        let display = state
                            .groups
                            .get(gi)
                            .map(|group| group.display.clone())
                            .unwrap_or_default();
                        let entries = state
                            .agents
                            .iter()
                            .map(|agent| {
                                PickerEntry::plain(
                                    agent.clone(),
                                    HubAction::LaunchAgent(gi, agent.clone()),
                                )
                            })
                            .collect();
                        state.picker = Some(HubPicker {
                            title: format!("Launch agent in {display}"),
                            crumbs: Vec::new(),
                            groups: vec![PickerGroup {
                                label: "Agents".to_owned(),
                                collapsed: false,
                                entries,
                            }],
                            selected: 1,
                            stage: PickerStage::Browse,
                        });
                    }
                });
            }
            (KeyCode::Char('+'), _) => {
                this.apply_picker_action(HubAction::PromptAddRuntime);
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                this.apply_picker_action(HubAction::PromptAddRepo);
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                let chosen = {
                    let state = self.state.lock();
                    state
                        .rows
                        .get(state.cursor)
                        .and_then(|row| match row.kind {
                            RowKind::Group(gi) | RowKind::Terminal(gi, _) => Some(gi),
                            _ => None,
                        })
                        .and_then(|gi| state.groups.get(gi))
                        .map(|group| (group.cwd.clone(), group.display.clone()))
                };
                match chosen {
                    Some((repo, display)) => {
                        this.apply_picker_action(HubAction::WorktreeRepoChosen { repo, display })
                    }
                    None => this.apply_picker_action(HubAction::PromptWorktreeRepo),
                }
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                this.open_actions_picker();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                self.set_status("refreshing…");
                this.spawn_refresh();
            }
            (KeyCode::Char('q'), KeyModifiers::NONE) => self.close_self(),
            _ => {}
        }
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }

    fn perform_actions(&self, _actions: Vec<termwiz::escape::Action>) {}

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        KeyboardEncoding::Xterm
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn kill(&self) {}

    fn palette(&self) -> ColorPalette {
        config::configuration().resolved_palette.clone().into()
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    fn is_mouse_grabbed(&self) -> bool {
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        None
    }
}

struct Sink;

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn open_orca_hub(term_window: &mut TermWindow, args: &OrcaHubArgs) -> anyhow::Result<bool> {
    let mux = Mux::get();
    let window_id = term_window.mux_window_id;
    let tab = mux
        .get_active_tab_for_window(window_id)
        .ok_or_else(|| anyhow!("no active tab"))?;
    let source = tab
        .get_active_pane()
        .ok_or_else(|| anyhow!("no active pane"))?;
    let source_pane_id = source.pane_id();

    let window = term_window
        .window
        .clone()
        .ok_or_else(|| anyhow!("no window handle"))?;

    let domain = if !args.domain.is_empty() {
        let domain = mux
            .get_domain_by_name(&args.domain)
            .ok_or_else(|| anyhow!("orca domain {} not found", args.domain))?;
        if domain.downcast_ref::<OrcaDomain>().is_none() {
            anyhow::bail!("domain {} is not an orca domain", args.domain);
        }
        Some(domain)
    } else {
        mux.iter_domains()
            .into_iter()
            .find(|d| d.downcast_ref::<OrcaDomain>().is_some())
    };

    enum Insertion {
        NewTab,
        Split {
            pane_index: usize,
            request: SplitRequest,
        },
    }

    let (insertion, pane_size) = if args.new_tab {
        (Insertion::NewTab, term_window.terminal_size())
    } else {
        let pane_index = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == source_pane_id)
            .map(|p| p.index)
            .ok_or_else(|| anyhow!("active pane not in tab"))?;
        let request = SplitRequest {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            size: match args.size {
                SplitSize::Percent(n) => MuxSplitSize::Percent(n),
                SplitSize::Cells(n) => MuxSplitSize::Cells(n),
            },
            top_level: false,
        };
        let split_size = tab
            .compute_split_size(pane_index, request)
            .ok_or_else(|| anyhow!("cannot compute split size"))?;
        (
            Insertion::Split {
                pane_index,
                request,
            },
            split_size.second,
        )
    };

    let pane = OrcaHubPane::new(domain, window, window_id, pane_size);

    let pane_dyn: Arc<dyn Pane> = pane.clone();
    let created_tab = match insertion {
        Insertion::NewTab => {
            let new_tab = Arc::new(MuxTab::new(&pane_size));
            new_tab.assign_pane(&pane_dyn);
            mux.add_tab_and_active_pane(&new_tab)?;
            mux.add_tab_to_window(&new_tab, window_id)?;
            true
        }
        Insertion::Split {
            pane_index,
            request,
        } => {
            mux.add_pane(&pane_dyn)?;
            tab.split_and_insert(pane_index, request, pane_dyn)?;
            false
        }
    };

    pane.start();
    if args.domain.is_empty() {
        pane.open_runtimes_picker();
    }
    Ok(created_tab)
}
