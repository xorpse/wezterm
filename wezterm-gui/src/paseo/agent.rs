use crate::termwindow::TermWindow;
use anyhow::anyhow;
use config::keyassignment::PaseoAgentArgs;
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
use paseo_client::{AgentStreamEvent, DaemonEvent, PaseoClient, TimelineItem, ToolCallDetail};
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
    match item.kind.as_str() {
        "user_message" => push_wrapped(&mut rows, "▸ ", &text, &attr_fg(AnsiColor::Teal), cols),
        "assistant_message" => push_wrapped(&mut rows, "", &text, &attr_default(), cols),
        "reasoning" => push_wrapped(&mut rows, "  ", &text, &attr_dim(), cols),
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
    match detail.kind.as_str() {
        "shell" => {
            if let Some(command) = &detail.command {
                push_wrapped(rows, "  $ ", command, &attr_fg(AnsiColor::Silver), cols);
            }
            if let Some(output) = &detail.output {
                push_wrapped(rows, "  ", &truncate_lines(output, 40), &attr_dim(), cols);
            }
        }
        "edit" => {
            if let Some(path) = &detail.path {
                push_wrapped(rows, "  edit ", path, &attr_fg(AnsiColor::Silver), cols);
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
        _ => {
            if let Some(path) = &detail.path {
                push_wrapped(rows, "  ", path, &attr_dim(), cols);
            }
            if let Some(text) = &detail.text {
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

fn item_key(item: &TimelineItem) -> Option<String> {
    if item.kind == "tool_call" {
        item.call_id.clone()
    } else {
        item.message_id.clone()
    }
}

struct AgentState {
    title: String,
    status_message: Option<String>,
    items: Vec<TimelineItem>,
    rows: Vec<AgentRow>,
    rows_version: u64,
    size: TerminalSize,
    seqno: SequenceNo,
    dead: bool,
}

impl AgentState {
    fn rebuild_rows(&mut self) {
        let cols = self.size.cols;
        let mut rows = Vec::new();
        if self.items.is_empty() {
            if let Some(message) = &self.status_message {
                push_wrapped(&mut rows, "", message, &attr_dim(), cols);
            }
        } else {
            for item in &self.items {
                item_to_rows(item, cols, &mut rows);
            }
        }
        self.rows = rows;
    }

    fn upsert(&mut self, item: TimelineItem) {
        match item_key(&item) {
            Some(key) => {
                if let Some(existing) = self
                    .items
                    .iter_mut()
                    .find(|it| item_key(it).as_deref() == Some(key.as_str()))
                {
                    *existing = item;
                } else {
                    self.items.push(item);
                }
            }
            None => self.items.push(item),
        }
    }

    fn row_line(&self, index: StableRowIndex) -> Line {
        if index >= 0 && (index as usize) < self.rows.len() {
            let row = &self.rows[index as usize];
            make_line(&row.text, &row.attrs, self.seqno, self.size.cols)
        } else {
            make_line("", &CellAttributes::default(), self.seqno, self.size.cols)
        }
    }

    fn total_rows(&self) -> usize {
        self.rows.len().max(self.size.rows)
    }
}

pub struct PaseoAgentPane {
    pane_id: PaneId,
    domain_id: DomainId,
    agent_id: Mutex<Option<String>>,
    client: PaseoClient,
    writer: Mutex<Vec<u8>>,
    window: Window,
    weak: Mutex<Weak<PaseoAgentPane>>,
    state: Mutex<AgentState>,
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
        if event.kind != "timeline" {
            return;
        }
        let Some(item) = &event.item else {
            return;
        };
        self.mutate(|state| {
            state.status_message = None;
            state.upsert(item.clone());
            state.rebuild_rows();
        });
    }

    pub fn start(self: &Arc<Self>, requested_agent: Option<String>) {
        let weak = Arc::downgrade(self);
        let client = self.client.clone();
        promise::spawn::spawn(async move {
            let agent_id = match resolve_agent(&client, requested_agent).await {
                Ok(id) => id,
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status("Agent (error)".to_string(), Some(format!("{err}")));
                    }
                    return;
                }
            };

            if let Some(pane) = weak.upgrade() {
                *pane.agent_id.lock() = Some(agent_id.clone());
                pane.set_status(format!("Agent {}", short_id(&agent_id)), None);
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
                if let DaemonEvent::AgentStream {
                    agent_id: stream_agent,
                    event,
                } = event
                {
                    if stream_agent == agent_id {
                        pane.apply_stream_event(&event);
                    }
                }
            }

            if let Some(pane) = weak.upgrade() {
                pane.state.lock().dead = true;
            }
        })
        .detach();
    }
}

async fn resolve_agent(client: &PaseoClient, requested: Option<String>) -> anyhow::Result<String> {
    if let Some(id) = requested {
        return Ok(id);
    }
    let agents = client.fetch_agents().await?;
    agents
        .into_iter()
        .find(|entry| entry.agent.archived_at.is_none())
        .map(|entry| entry.agent.id)
        .ok_or_else(|| anyhow!("no agents available"))
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
        StableCursorPosition {
            x: 0,
            y: 0,
            shape: termwiz::surface::CursorShape::Default,
            visibility: CursorVisibility::Hidden,
        }
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let state = self.state.lock();
        let mut out = Vec::new();
        for index in lines.start..lines.end {
            out.push(state.row_line(index));
        }
        (lines.start, out)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let state = self.state.lock();
        let mut built: Vec<Line> = (lines.start..lines.end)
            .map(|index| state.row_line(index))
            .collect();
        let mut refs: Vec<&mut Line> = built.iter_mut().collect();
        with_lines.with_lines_mut(lines.start, &mut refs);
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
        let total = state.total_rows();
        let viewport = state.size.rows;
        RenderableDimensions {
            cols: state.size.cols,
            viewport_rows: viewport,
            scrollback_rows: total,
            physical_top: total.saturating_sub(viewport) as StableRowIndex,
            scrollback_top: 0,
            dpi: state.size.dpi,
            pixel_width: state.size.pixel_width,
            pixel_height: state.size.pixel_height,
            reverse_video: false,
        }
    }

    fn get_title(&self) -> String {
        self.state.lock().title.clone()
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

    fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn mouse_event(&self, _event: wezterm_term::MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.state.lock().dead
    }

    fn palette(&self) -> ColorPalette {
        config::configuration().resolved_palette.clone().into()
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
    let paseo_domain = domain
        .downcast_ref::<paseo_mux::PaseoDomain>()
        .ok_or_else(|| anyhow!("domain {} is not a paseo domain", args.domain))?;
    let client = paseo_domain
        .client()
        .ok_or_else(|| anyhow!("attach the {} domain before opening an agent", args.domain))?;

    let pane = Arc::new(PaseoAgentPane {
        pane_id: alloc_pane_id(),
        domain_id: source.domain_id(),
        agent_id: Mutex::new(None),
        client,
        writer: Mutex::new(Vec::new()),
        window,
        weak: Mutex::new(Weak::new()),
        state: Mutex::new(AgentState {
            title: "Agent (loading…)".to_string(),
            status_message: Some("⟳ loading agent…".to_string()),
            items: Vec::new(),
            rows: Vec::new(),
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

    pane.start(args.agent_id.clone());

    Ok(())
}
