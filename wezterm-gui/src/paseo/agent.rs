use crate::termwindow::{TermWindow, TermWindowNotif};
use anyhow::anyhow;
use config::keyassignment::{KeyAssignment, PaseoAgentArgs};
use mux::domain::DomainId;
use mux::pane::{
    alloc_pane_id, impl_for_each_logical_line_via_get_logical_lines,
    impl_get_logical_lines_via_get_lines, CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane,
    PaneId, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{SplitDirection, SplitRequest, SplitSize as MuxSplitSize};
use mux::Mux;
use parking_lot::Mutex;
use paseo_client::{
    AgentMode, AgentSnapshot, AgentStreamEvent, DaemonEvent, ModelDefinition, PaseoClient,
    PermissionRequest, PermissionResponse, TimelineItem, ToolCallDetail,
};
use rangeset::RangeSet;
use std::ops::Range;
use std::sync::{Arc, Weak};
use termwiz::cell::{CellAttributes, Intensity};
use termwiz::color::AnsiColor;
use termwiz::surface::{CursorVisibility, Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{unicode_column_width, KeyCode, KeyModifiers, StableRowIndex, TerminalSize};
use window::{Window, WindowOps};

fn make_line(text: &str, attrs: &CellAttributes, seqno: SequenceNo, cols: usize) -> Line {
    let width = unicode_column_width(text, None);
    let padded = if width < cols {
        format!("{text}{}", " ".repeat(cols - width))
    } else {
        text.to_string()
    };
    Line::from_text(&padded, attrs, seqno, None)
}

fn attr_default() -> CellAttributes {
    CellAttributes::default()
}

fn attr_dim() -> CellAttributes {
    let mut a = CellAttributes::default();
    a.set_intensity(Intensity::Half);
    a
}

fn attr_fg(color: AnsiColor) -> CellAttributes {
    let mut a = CellAttributes::default();
    a.set_foreground(color);
    a
}

fn attr_bold_fg(color: AnsiColor) -> CellAttributes {
    let mut a = CellAttributes::default();
    a.set_intensity(Intensity::Bold);
    a.set_foreground(color);
    a
}

struct AgentRow {
    text: String,
    attrs: CellAttributes,
}

fn blank_row() -> AgentRow {
    AgentRow {
        text: String::new(),
        attrs: attr_default(),
    }
}

fn push_wrapped(
    rows: &mut Vec<AgentRow>,
    prefix: &str,
    text: &str,
    attrs: &CellAttributes,
    cols: usize,
) {
    let indent: String = " ".repeat(prefix.chars().count());
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push(AgentRow {
                text: prefix.to_string(),
                attrs: attrs.clone(),
            });
            continue;
        }
        let width = cols.saturating_sub(prefix.chars().count()).max(1);
        let mut idx = 0;
        let mut first = true;
        while idx < chars.len() {
            let end = (idx + width).min(chars.len());
            let chunk: String = chars[idx..end].iter().collect();
            let p = if first { prefix } else { indent.as_str() };
            rows.push(AgentRow {
                text: format!("{p}{chunk}"),
                attrs: attrs.clone(),
            });
            first = false;
            idx = end;
        }
    }
}

fn item_to_rows(item: &TimelineItem, cols: usize, out: &mut Vec<AgentRow>) {
    let mut rows = Vec::new();
    let text = item.text.clone().unwrap_or_default();
    let trimmed = text.trim();
    match item.kind.as_str() {
        "user_message" => push_wrapped(&mut rows, "▸ ", trimmed, &attr_fg(AnsiColor::Teal), cols),
        "assistant_message" => push_wrapped(&mut rows, "", trimmed, &attr_default(), cols),
        "reasoning" => push_wrapped(&mut rows, "  ", trimmed, &attr_dim(), cols),
        "error" => push_wrapped(
            &mut rows,
            "error: ",
            &text,
            &attr_fg(AnsiColor::Maroon),
            cols,
        ),
        "tool_call" => tool_call_rows(item, cols, &mut rows),
        "compaction" => push_wrapped(&mut rows, "— ", "context compacted", &attr_dim(), cols),
        _ => {
            if !text.is_empty() {
                push_wrapped(&mut rows, "", &text, &attr_dim(), cols);
            }
        }
    }
    if !rows.is_empty() {
        out.append(&mut rows);
        out.push(blank_row());
    }
}

fn tool_call_rows(item: &TimelineItem, cols: usize, rows: &mut Vec<AgentRow>) {
    let name = item.name.clone().unwrap_or_else(|| "tool".to_string());
    let status = item.status.clone().unwrap_or_default();
    let glyph = match status.as_str() {
        "completed" => "✓",
        "failed" => "✗",
        "canceled" => "⊘",
        _ => "•",
    };
    rows.push(AgentRow {
        text: format!("{glyph} {name}"),
        attrs: attr_bold_fg(AnsiColor::Blue),
    });
    if let Some(detail) = &item.detail {
        tool_detail_rows(detail, cols, rows);
    }
}

fn tool_detail_rows(detail: &ToolCallDetail, cols: usize, rows: &mut Vec<AgentRow>) {
    let target = attr_fg(AnsiColor::Silver);
    match detail.kind.as_str() {
        "shell" => {
            if let Some(command) = &detail.command {
                push_wrapped(rows, "  $ ", command, &target, cols);
            }
            if let Some(output) = &detail.output {
                push_wrapped(rows, "  ", &truncate_lines(output, 40), &attr_dim(), cols);
            }
        }
        "read" => {
            if let Some(path) = &detail.file_path {
                push_wrapped(rows, "  read ", path, &target, cols);
            }
        }
        "write" => {
            if let Some(path) = &detail.file_path {
                push_wrapped(rows, "  write ", path, &target, cols);
            }
        }
        "edit" => {
            if let Some(path) = &detail.file_path {
                push_wrapped(rows, "  edit ", path, &target, cols);
            }
            if let Some(diff) = &detail.unified_diff {
                for line in truncate_lines(diff, 60).split('\n') {
                    let attrs = if line.starts_with('+') {
                        attr_fg(AnsiColor::Green)
                    } else if line.starts_with('-') {
                        attr_fg(AnsiColor::Maroon)
                    } else {
                        attr_dim()
                    };
                    push_wrapped(rows, "  ", line, &attrs, cols);
                }
            }
        }
        "search" => {
            if let Some(query) = &detail.query {
                push_wrapped(rows, "  search ", query, &target, cols);
            }
        }
        "fetch" => {
            if let Some(url) = &detail.url {
                push_wrapped(rows, "  fetch ", url, &target, cols);
            }
        }
        "sub_agent" => {
            if let Some(description) = &detail.description {
                push_wrapped(rows, "  ", description, &attr_dim(), cols);
            }
        }
        _ => {
            if let Some(path) = &detail.file_path {
                push_wrapped(rows, "  ", path, &target, cols);
            }
            if let Some(text) = detail.text.as_ref().or(detail.content.as_ref()) {
                push_wrapped(rows, "  ", &truncate_lines(text, 20), &attr_dim(), cols);
            }
        }
    }
}

fn truncate_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        text.trim_end().to_string()
    } else {
        let mut out = lines[..max].join("\n");
        out.push_str(&format!("\n… ({} more lines)", lines.len() - max));
        out
    }
}

fn is_message(kind: &str) -> bool {
    matches!(kind, "assistant_message" | "reasoning" | "user_message")
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Scroll,
    Compose,
}

struct PickerEntry {
    id: String,
    label: String,
}

struct PickerState {
    entries: Vec<PickerEntry>,
    selected: usize,
}

fn picker_label(agent: &AgentSnapshot) -> String {
    let title = agent.title.clone().unwrap_or_default();
    let title = if title.is_empty() {
        short_id(&agent.id)
    } else {
        title.replace('\n', " ").chars().take(60).collect()
    };
    format!("[{}] {} ({})", agent.status, title, agent.provider)
}

struct AgentState {
    title: String,
    status_message: Option<String>,
    items: Vec<TimelineItem>,
    pending: Option<PermissionRequest>,
    picker: Option<PickerState>,
    mode: Mode,
    composer: String,
    provider: String,
    agent_status: String,
    requires_attention: bool,
    model: Option<String>,
    current_mode_id: Option<String>,
    available_modes: Vec<AgentMode>,
    thinking_option_id: Option<String>,
    models: Vec<ModelDefinition>,
    transcript: Vec<AgentRow>,
    footer: Vec<AgentRow>,
    scroll: usize,
    follow: bool,
    rows_version: u64,
    size: TerminalSize,
    seqno: SequenceNo,
    dead: bool,
}

impl AgentState {
    fn mode_label(&self) -> String {
        let id = self.current_mode_id.clone().unwrap_or_default();
        self.available_modes
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or(id)
    }

    fn effort_label(&self) -> String {
        let id = self.thinking_option_id.clone().unwrap_or_default();
        if id.is_empty() {
            return "—".to_string();
        }
        self.current_model()
            .and_then(|m| m.thinking_options.iter().find(|o| o.id == id))
            .map(|o| o.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or(id)
    }

    fn current_model(&self) -> Option<&ModelDefinition> {
        let id = self.model.as_deref()?;
        self.models.iter().find(|m| m.id == id)
    }
}

impl AgentState {
    fn rebuild_rows(&mut self) {
        let cols = self.size.cols;

        if let Some(picker) = &self.picker {
            let mut transcript = Vec::new();
            transcript.push(AgentRow {
                text: "Select an agent:".to_string(),
                attrs: attr_bold_fg(AnsiColor::Teal),
            });
            transcript.push(blank_row());
            if picker.entries.is_empty() {
                push_wrapped(&mut transcript, "  ", "no agents", &attr_dim(), cols);
            }
            for (i, entry) in picker.entries.iter().enumerate() {
                let (prefix, attrs) = if i == picker.selected {
                    ("▸ ", attr_bold_fg(AnsiColor::Teal))
                } else {
                    ("  ", attr_default())
                };
                push_wrapped(&mut transcript, prefix, &entry.label, &attrs, cols);
            }
            self.transcript = transcript;
            self.footer = vec![AgentRow {
                text: "❯ (Enter: open · j/k: move · q: close)".to_string(),
                attrs: attr_dim(),
            }];
            self.clamp_scroll();
            return;
        }

        let mut transcript = Vec::new();
        if self.items.is_empty() {
            if let Some(message) = &self.status_message {
                push_wrapped(&mut transcript, "", message, &attr_dim(), cols);
            }
        } else {
            for item in &self.items {
                item_to_rows(item, cols, &mut transcript);
            }
        }
        self.transcript = transcript;

        let mut footer = Vec::new();
        if let Some(request) = &self.pending {
            let title = request
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| request.name.clone());
            push_wrapped(
                &mut footer,
                "⚠ permission: ",
                &title,
                &attr_bold_fg(AnsiColor::Yellow),
                cols,
            );
            push_wrapped(
                &mut footer,
                "  ",
                "[y] allow   [n] deny",
                &attr_fg(AnsiColor::Yellow),
                cols,
            );
        }
        if !self.agent_status.is_empty() || self.model.is_some() {
            let model = self.model.clone().unwrap_or_else(|| "—".to_string());
            footer.push(AgentRow {
                text: format!(
                    "[{}]  mode:{}  model:{}  effort:{}",
                    self.agent_status,
                    self.mode_label(),
                    model,
                    self.effort_label()
                ),
                attrs: attr_fg(AnsiColor::Teal),
            });
            footer.push(AgentRow {
                text: "m:mode  M:model  e:effort  x:stop".to_string(),
                attrs: attr_dim(),
            });
        }
        let composer = match self.mode {
            Mode::Compose => AgentRow {
                text: format!("❯ {}", self.composer),
                attrs: attr_default(),
            },
            Mode::Scroll => AgentRow {
                text: "❯ (i: type · j/k: scroll · g/G: top/bottom · q: close)".to_string(),
                attrs: attr_dim(),
            },
        };
        footer.push(composer);
        self.footer = footer;

        self.clamp_scroll();
    }

    fn view_rows(&self) -> usize {
        self.size.rows.saturating_sub(self.footer.len())
    }

    fn max_scroll(&self) -> usize {
        self.transcript.len().saturating_sub(self.view_rows())
    }

    fn clamp_scroll(&mut self) {
        if self.follow {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    fn composer_screen_row(&self) -> usize {
        self.size.rows.saturating_sub(1)
    }

    fn row_line(&self, screen_row: usize) -> Line {
        let cols = self.size.cols;
        let view_rows = self.view_rows();
        let row = if screen_row < view_rows {
            self.transcript.get(self.scroll + screen_row)
        } else {
            self.footer.get(screen_row - view_rows)
        };
        match row {
            Some(row) => make_line(&row.text, &row.attrs, self.seqno, cols),
            None => make_line("", &CellAttributes::default(), self.seqno, cols),
        }
    }

    fn apply_live_item(&mut self, item: TimelineItem) {
        if item.kind == "tool_call" {
            if let Some(call_id) = item.call_id.clone() {
                if let Some(existing) = self
                    .items
                    .iter_mut()
                    .find(|it| it.call_id.as_deref() == Some(call_id.as_str()))
                {
                    *existing = item;
                    return;
                }
            }
            self.items.push(item);
            return;
        }

        if is_message(&item.kind) {
            if let Some(last) = self.items.last_mut() {
                if last.kind == item.kind
                    && last.message_id.is_some()
                    && last.message_id == item.message_id
                {
                    let old = last.text.clone().unwrap_or_default();
                    let new = item.text.clone().unwrap_or_default();
                    last.text = Some(if new.starts_with(&old) {
                        new
                    } else {
                        format!("{old}{new}")
                    });
                    return;
                }
            }
        }

        self.items.push(item);
    }
}

pub struct PaseoAgentPane {
    pane_id: PaneId,
    domain_id: DomainId,
    agent_id: Mutex<Option<String>>,
    domain: Arc<dyn mux::domain::Domain>,
    client: Mutex<Option<PaseoClient>>,
    writer: Mutex<Vec<u8>>,
    window: Window,
    weak: Mutex<Weak<PaseoAgentPane>>,
    state: Mutex<AgentState>,
}

impl PaseoAgentPane {
    fn client(&self) -> Option<PaseoClient> {
        self.client.lock().clone()
    }
}

impl PaseoAgentPane {
    fn mutate<F: FnOnce(&mut AgentState)>(&self, f: F) {
        {
            let mut state = self.state.lock();
            f(&mut state);
            state.rows_version += 1;
            state.seqno += 1;
        }
        self.window.invalidate();
    }

    fn set_timeline(&self, items: &[TimelineItem]) {
        self.mutate(|state| {
            state.status_message = None;
            state.items = items.to_vec();
            state.rebuild_rows();
        });
    }

    fn set_status(&self, title: String, message: Option<String>) {
        self.mutate(|state| {
            state.title = title;
            if let Some(message) = message {
                state.items.clear();
                state.status_message = Some(message);
                state.rebuild_rows();
            }
        });
    }

    fn apply_stream_event(&self, event: &AgentStreamEvent) {
        match event.kind.as_str() {
            "turn_started" => {
                self.mutate(|state| {
                    state.agent_status = "running".to_string();
                    state.rebuild_rows();
                });
                return;
            }
            "turn_completed" | "turn_canceled" => {
                self.mutate(|state| {
                    state.agent_status = "idle".to_string();
                    state.rebuild_rows();
                });
                return;
            }
            "turn_failed" => {
                self.mutate(|state| {
                    state.agent_status = "error".to_string();
                    state.rebuild_rows();
                });
                return;
            }
            "timeline" => {}
            _ => return,
        }
        let Some(item) = &event.item else {
            return;
        };
        self.mutate(|state| {
            state.status_message = None;
            state.apply_live_item(item.clone());
            state.rebuild_rows();
        });
    }

    fn set_pending(&self, request: PermissionRequest) {
        self.mutate(|state| {
            state.pending = Some(request);
            state.rebuild_rows();
        });
    }

    fn set_snapshot(&self, snapshot: &AgentSnapshot) {
        self.mutate(|state| {
            state.provider = snapshot.provider.clone();
            state.agent_status = snapshot.status.clone();
            state.model = snapshot.model.clone();
            state.current_mode_id = snapshot.current_mode_id.clone();
            state.available_modes = snapshot.available_modes.clone();
            state.thinking_option_id = snapshot.thinking_option_id.clone();
            state.requires_attention = snapshot.requires_attention;
            state.rebuild_rows();
        });
    }

    fn set_models(&self, models: Vec<ModelDefinition>) {
        self.mutate(|state| {
            state.models = models;
            state.rebuild_rows();
        });
    }

    fn agent_id(&self) -> Option<String> {
        self.agent_id.lock().clone()
    }

    fn refresh_after<F>(&self, action: F)
    where
        F: FnOnce(
                PaseoClient,
                String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + 'static,
    {
        let Some(agent_id) = self.agent_id() else {
            return;
        };
        let Some(client) = self.client() else {
            return;
        };
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            action(client.clone(), agent_id.clone()).await;
            if let Ok(snapshot) = client.fetch_agent(&agent_id).await {
                if let Some(pane) = weak.upgrade() {
                    pane.set_snapshot(&snapshot);
                }
            }
        })
        .detach();
    }

    fn stop(&self) {
        self.refresh_after(|client, agent_id| {
            Box::pin(async move {
                let _ = client.cancel_agent(&agent_id).await;
            })
        });
    }

    fn cycle_mode(&self) {
        let (agent_id, next) = {
            let state = self.state.lock();
            let next = cycle_next(
                &state
                    .available_modes
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>(),
                state.current_mode_id.as_deref(),
            );
            (self.agent_id(), next)
        };
        let _ = agent_id;
        if let Some(next) = next {
            self.refresh_after(move |client, agent_id| {
                Box::pin(async move {
                    let _ = client.set_agent_mode(&agent_id, &next).await;
                })
            });
        }
    }

    fn cycle_model(&self) {
        let (agent_id, next) = {
            let state = self.state.lock();
            let next = cycle_next(
                &state
                    .models
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>(),
                state.model.as_deref(),
            );
            (self.agent_id(), next)
        };
        let _ = agent_id;
        if let Some(next) = next {
            self.refresh_after(move |client, agent_id| {
                Box::pin(async move {
                    let _ = client.set_agent_model(&agent_id, &next).await;
                })
            });
        }
    }

    fn cycle_effort(&self) {
        let (agent_id, next) = {
            let state = self.state.lock();
            let options: Vec<String> = state
                .current_model()
                .map(|m| m.thinking_options.iter().map(|o| o.id.clone()).collect())
                .unwrap_or_default();
            let next = cycle_next(&options, state.thinking_option_id.as_deref());
            (self.agent_id(), next)
        };
        let _ = agent_id;
        if let Some(next) = next {
            self.refresh_after(move |client, agent_id| {
                Box::pin(async move {
                    let _ = client.set_agent_thinking(&agent_id, &next).await;
                })
            });
        }
    }

    fn scroll_lines(&self, delta: isize) {
        self.mutate(|state| {
            if delta < 0 {
                state.follow = false;
                state.scroll = state.scroll.saturating_sub((-delta) as usize);
            } else {
                let max = state.max_scroll();
                state.scroll = (state.scroll + delta as usize).min(max);
                state.follow = state.scroll >= max;
            }
        });
    }

    fn scroll_page(&self, dir: isize) {
        let page = self.state.lock().view_rows().max(1) as isize;
        self.scroll_lines(dir * page);
    }

    fn scroll_to_top(&self) {
        self.mutate(|state| {
            state.follow = false;
            state.scroll = 0;
        });
    }

    fn scroll_to_bottom(&self) {
        self.mutate(|state| {
            state.follow = true;
            state.scroll = state.max_scroll();
        });
    }

    fn close(&self) {
        let pane_id = self.pane_id;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |tw| {
                if let Some(pane) = Mux::get().get_pane(pane_id) {
                    let _ = tw.perform_key_assignment(
                        &pane,
                        &KeyAssignment::CloseCurrentPane { confirm: false },
                    );
                }
            })));
    }

    fn submit_composer(&self) {
        let text = {
            let mut state = self.state.lock();
            std::mem::take(&mut state.composer).trim().to_string()
        };
        if !text.is_empty() {
            if let (Some(agent_id), Some(client)) = (self.agent_id.lock().clone(), self.client()) {
                promise::spawn::spawn(async move {
                    let _ = client.send_agent_message(&agent_id, &text).await;
                })
                .detach();
            }
        }
        self.mutate(|state| state.rebuild_rows());
    }

    fn respond_permission(&self, allow: bool) {
        let (agent_id, request_id, action_id) = {
            let state = self.state.lock();
            let Some(request) = &state.pending else {
                return;
            };
            let behavior = if allow { "allow" } else { "deny" };
            let action_id = request
                .actions
                .iter()
                .find(|a| a.behavior == behavior)
                .map(|a| a.id.clone());
            (self.agent_id.lock().clone(), request.id.clone(), action_id)
        };
        if let (Some(agent_id), Some(client)) = (agent_id, self.client()) {
            let response = if allow {
                PermissionResponse::Allow {
                    selected_action_id: action_id,
                }
            } else {
                PermissionResponse::Deny {
                    message: None,
                    interrupt: false,
                }
            };
            promise::spawn::spawn(async move {
                let _ = client
                    .respond_permission(&agent_id, &request_id, response)
                    .await;
            })
            .detach();
        }
        self.mutate(|state| {
            state.pending = None;
            state.rebuild_rows();
        });
    }

    pub fn start(self: &Arc<Self>, source: AgentSource) {
        let weak = Arc::downgrade(self);
        let domain = self.domain.clone();
        promise::spawn::spawn(async move {
            let client = match ensure_domain_client(&domain).await {
                Ok(client) => client,
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status(
                            "Agent (error)".to_string(),
                            Some(format!("connect failed: {err}")),
                        );
                    }
                    return;
                }
            };
            if let Some(pane) = weak.upgrade() {
                *pane.client.lock() = Some(client.clone());
            }

            let _ = client.subscribe_agents().await;

            if source.agent_id.is_none() && source.provider.is_none() {
                match client.fetch_agents().await {
                    Ok(agents) => {
                        let entries: Vec<PickerEntry> = agents
                            .into_iter()
                            .filter(|e| e.agent.archived_at.is_none())
                            .map(|e| PickerEntry {
                                id: e.agent.id.clone(),
                                label: picker_label(&e.agent),
                            })
                            .collect();
                        if let Some(pane) = weak.upgrade() {
                            pane.enter_picker(entries);
                        }
                    }
                    Err(err) => {
                        if let Some(pane) = weak.upgrade() {
                            pane.set_status("Agent (error)".to_string(), Some(format!("{err}")));
                        }
                    }
                }
                return;
            }

            match resolve_or_create(&client, source).await {
                Ok(snapshot) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.load_agent(snapshot);
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status("Agent (error)".to_string(), Some(format!("{err}")));
                    }
                }
            }
        })
        .detach();
    }

    fn load_agent(self: &Arc<Self>, snapshot: AgentSnapshot) {
        let agent_id = snapshot.id.clone();
        let provider = snapshot.provider.clone();
        let cwd = snapshot.cwd.clone();
        *self.agent_id.lock() = Some(agent_id.clone());
        self.mutate(|state| {
            state.picker = None;
            state.title = format!("Agent {}", short_id(&agent_id));
        });
        self.set_snapshot(&snapshot);

        let weak = Arc::downgrade(self);
        let Some(client) = self.client() else {
            return;
        };
        promise::spawn::spawn(async move {
            if !provider.is_empty() {
                if let Ok(models) = client.list_provider_models(&provider, Some(&cwd)).await {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_models(models);
                    }
                }
            }

            let _ = client.set_timeline_subscription(&[agent_id.clone()]).await;

            match client.fetch_agent_timeline(&agent_id, "tail", 200).await {
                Ok(items) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_timeline(&items);
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status(
                            format!("Agent {}", short_id(&agent_id)),
                            Some(format!("timeline error: {err}")),
                        );
                    }
                }
            }

            let mut events = client.events();
            while let Ok(event) = events.recv().await {
                let Some(pane) = weak.upgrade() else {
                    break;
                };
                match event {
                    DaemonEvent::AgentStream {
                        agent_id: stream_agent,
                        event,
                    } if stream_agent == agent_id => {
                        pane.apply_stream_event(&event);
                    }
                    DaemonEvent::PermissionRequest {
                        agent_id: perm_agent,
                        request,
                    } if perm_agent == agent_id => {
                        pane.set_pending(*request);
                    }
                    DaemonEvent::AgentUpsert(snapshot) if snapshot.id == agent_id => {
                        pane.set_snapshot(&snapshot);
                    }
                    DaemonEvent::Disconnected => {
                        pane.mutate(|state| {
                            state.agent_status = "disconnected".to_string();
                            state.rebuild_rows();
                        });
                        break;
                    }
                    _ => {}
                }
            }

            if let Some(pane) = weak.upgrade() {
                pane.state.lock().dead = true;
            }
        })
        .detach();
    }

    fn enter_picker(&self, entries: Vec<PickerEntry>) {
        self.mutate(|state| {
            state.status_message = None;
            state.picker = Some(PickerState {
                entries,
                selected: 0,
            });
            state.rebuild_rows();
        });
    }

    fn picker_move(&self, delta: isize) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                if picker.entries.is_empty() {
                    return;
                }
                let len = picker.entries.len() as isize;
                let next = (picker.selected as isize + delta).rem_euclid(len);
                picker.selected = next as usize;
            }
            state.rebuild_rows();
        });
    }

    fn picker_select(&self) {
        let chosen = {
            let state = self.state.lock();
            state
                .picker
                .as_ref()
                .and_then(|p| p.entries.get(p.selected).map(|e| e.id.clone()))
        };
        let Some(agent_id) = chosen else {
            return;
        };
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.status_message = Some("⟳ loading agent…".to_string());
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            match client.fetch_agent(&agent_id).await {
                Ok(snapshot) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.load_agent(snapshot);
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status(
                            "Agent (error)".to_string(),
                            Some(format!("load failed: {err}")),
                        );
                    }
                }
            }
        })
        .detach();
    }
}

pub struct AgentSource {
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub cwd: Option<String>,
    pub prompt: Option<String>,
}

async fn ensure_domain_client(
    domain: &Arc<dyn mux::domain::Domain>,
) -> anyhow::Result<PaseoClient> {
    let paseo = domain
        .downcast_ref::<paseo_mux::PaseoDomain>()
        .ok_or_else(|| anyhow!("not a paseo domain"))?;
    paseo.ensure_client().await
}

async fn resolve_or_create(
    client: &PaseoClient,
    source: AgentSource,
) -> anyhow::Result<AgentSnapshot> {
    if let (Some(provider), Some(cwd)) = (source.provider.as_ref(), source.cwd.as_ref()) {
        let workspace = client.open_project(cwd).await?;
        return client
            .create_agent(provider, cwd, Some(&workspace), source.prompt.as_deref())
            .await
            .map_err(anyhow::Error::from);
    }

    let agents = client.fetch_agents().await?;
    let entry = match source.agent_id {
        Some(id) => agents.into_iter().find(|entry| entry.agent.id == id),
        None => agents
            .into_iter()
            .find(|entry| entry.agent.archived_at.is_none()),
    };
    entry
        .map(|entry| entry.agent)
        .ok_or_else(|| anyhow!("no agents available"))
}

fn cycle_next(options: &[String], current: Option<&str>) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let idx = current
        .and_then(|c| options.iter().position(|o| o == c))
        .unwrap_or(0);
    Some(options[(idx + 1) % options.len()].clone())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

impl Pane for PaseoAgentPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
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
            for r in lines.start.max(0)..lines.end {
                set.add(r);
            }
        }
        set
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let state = self.state.lock();
        if state.mode == Mode::Compose {
            return StableCursorPosition {
                x: 2 + state.composer.chars().count(),
                y: state.composer_screen_row() as StableRowIndex,
                shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: CursorVisibility::Visible,
            };
        }
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
        let mut built: Vec<Line> = (start..lines.end.max(start))
            .map(|index| state.row_line(index as usize))
            .collect();
        let mut refs: Vec<&mut Line> = built.iter_mut().collect();
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
        let state = self.state.lock();
        let glyph = if state.pending.is_some() || state.requires_attention {
            "⚠ "
        } else {
            match state.agent_status.as_str() {
                "running" => "● ",
                "error" => "✗ ",
                _ => "",
            }
        };
        format!("{glyph}{}", state.title)
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> parking_lot::MappedMutexGuard<'_, dyn std::io::Write> {
        parking_lot::MutexGuard::map(self.writer.lock(), |w| {
            let w: &mut dyn std::io::Write = w;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.mutate(|state| {
            state.size = size;
            state.rebuild_rows();
        });
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        if self.state.lock().picker.is_some() {
            match key {
                KeyCode::Char('j') | KeyCode::DownArrow => self.picker_move(1),
                KeyCode::Char('k') | KeyCode::UpArrow => self.picker_move(-1),
                KeyCode::Char('\r') | KeyCode::Enter => self.picker_select(),
                KeyCode::Char('q') | KeyCode::Escape => self.close(),
                _ => {}
            }
            return Ok(());
        }
        let mode = self.state.lock().mode;
        match mode {
            Mode::Compose => match key {
                KeyCode::Char('\r') | KeyCode::Enter => self.submit_composer(),
                KeyCode::Backspace => self.mutate(|state| {
                    state.composer.pop();
                    state.rebuild_rows();
                }),
                KeyCode::Escape => self.mutate(|state| {
                    state.mode = Mode::Scroll;
                    state.rebuild_rows();
                }),
                KeyCode::Char(c) if !c.is_control() && !mods.contains(KeyModifiers::CTRL) => self
                    .mutate(|state| {
                        state.composer.push(c);
                        state.rebuild_rows();
                    }),
                _ => {}
            },
            Mode::Scroll => match key {
                KeyCode::Char('i') | KeyCode::Char('\r') | KeyCode::Enter => {
                    self.mutate(|state| {
                        state.mode = Mode::Compose;
                        state.rebuild_rows();
                    });
                    self.scroll_to_bottom();
                }
                KeyCode::Char('q') => self.close(),
                KeyCode::Char('y') => self.respond_permission(true),
                KeyCode::Char('n') => self.respond_permission(false),
                KeyCode::Char('x') => self.stop(),
                KeyCode::Char('c') if mods.contains(KeyModifiers::CTRL) => self.stop(),
                KeyCode::Char('m') => self.cycle_mode(),
                KeyCode::Char('M') => self.cycle_model(),
                KeyCode::Char('e') => self.cycle_effort(),
                KeyCode::Char('g') | KeyCode::Home => self.scroll_to_top(),
                KeyCode::Char('G') | KeyCode::End => self.scroll_to_bottom(),
                KeyCode::PageDown => self.scroll_page(1),
                KeyCode::PageUp => self.scroll_page(-1),
                KeyCode::Char('j') | KeyCode::DownArrow => self.scroll_lines(3),
                KeyCode::Char('k') | KeyCode::UpArrow => self.scroll_lines(-3),
                _ => {}
            },
        }
        Ok(())
    }

    fn mouse_event(&self, event: wezterm_term::MouseEvent) -> anyhow::Result<()> {
        use wezterm_term::{MouseButton, MouseEventKind};
        if event.kind == MouseEventKind::Press {
            match event.button {
                MouseButton::WheelUp(n) => self.scroll_lines(-(n.max(1) as isize)),
                MouseButton::WheelDown(n) => self.scroll_lines(n.max(1) as isize),
                _ => {}
            }
        }
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.state.lock().dead
    }

    fn palette(&self) -> ColorPalette {
        config::configuration().resolved_palette.clone().into()
    }

    fn is_mouse_grabbed(&self) -> bool {
        true
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        None
    }
}

pub fn open_paseo_agent_pane(
    term_window: &mut TermWindow,
    args: &PaseoAgentArgs,
) -> anyhow::Result<()> {
    let mux = Mux::get();
    let tab = mux
        .get_active_tab_for_window(term_window.mux_window_id)
        .ok_or_else(|| anyhow!("no active tab"))?;
    let source = tab
        .get_active_pane()
        .ok_or_else(|| anyhow!("no active pane"))?;
    let source_pane_id = source.pane_id();

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
            config::keyassignment::SplitSize::Percent(n) => MuxSplitSize::Percent(n),
            config::keyassignment::SplitSize::Cells(n) => MuxSplitSize::Cells(n),
        },
        top_level: false,
    };

    let split_size = tab
        .compute_split_size(pane_index, request)
        .ok_or_else(|| anyhow!("cannot compute split size"))?;

    let window = term_window
        .window
        .clone()
        .ok_or_else(|| anyhow!("no window handle"))?;

    let domain = mux
        .get_domain_by_name(&args.domain)
        .ok_or_else(|| anyhow!("paseo domain {} not found", args.domain))?;
    if domain.downcast_ref::<paseo_mux::PaseoDomain>().is_none() {
        anyhow::bail!("domain {} is not a paseo domain", args.domain);
    }

    let pane = Arc::new(PaseoAgentPane {
        pane_id: alloc_pane_id(),
        domain_id: source.domain_id(),
        agent_id: Mutex::new(None),
        domain: domain.clone(),
        client: Mutex::new(None),
        writer: Mutex::new(Vec::new()),
        window,
        weak: Mutex::new(Weak::new()),
        state: Mutex::new(AgentState {
            title: "Agent (loading…)".to_string(),
            status_message: Some("⟳ loading agent…".to_string()),
            items: Vec::new(),
            pending: None,
            picker: None,
            mode: Mode::Scroll,
            composer: String::new(),
            provider: String::new(),
            agent_status: String::new(),
            requires_attention: false,
            model: None,
            current_mode_id: None,
            available_modes: Vec::new(),
            thinking_option_id: None,
            models: Vec::new(),
            transcript: Vec::new(),
            footer: Vec::new(),
            scroll: 0,
            follow: true,
            rows_version: 0,
            size: split_size.second,
            seqno: 1,
            dead: false,
        }),
    });

    pane.mutate(|state| state.rebuild_rows());
    *pane.weak.lock() = Arc::downgrade(&pane);

    let pane_dyn: Arc<dyn Pane> = pane.clone();
    mux.add_pane(&pane_dyn)?;
    tab.split_and_insert(pane_index, request, pane_dyn)?;

    pane.start(AgentSource {
        agent_id: args.agent_id.clone(),
        provider: args.provider.clone(),
        cwd: args.cwd.clone(),
        prompt: args.prompt.clone(),
    });

    Ok(())
}
