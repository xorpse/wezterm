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
use mux::tab::{SplitDirection, SplitRequest, SplitSize as MuxSplitSize, Tab as MuxTab};
use mux::Mux;
use parking_lot::Mutex;
use paseo_client::{
    AgentListEntry, AgentMode, AgentSnapshot, AgentStreamEvent, DaemonEvent, ModelDefinition,
    PaseoClient, PermissionRequest, PermissionResponse, TimelineItem, ToolCallDetail, Workspace,
};
use rangeset::RangeSet;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Weak};
use termwiz::cell::{Cell, CellAttributes, Intensity, Underline};
use termwiz::color::{AnsiColor, ColorAttribute, SrgbaTuple};
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

#[derive(Clone)]
struct AgentRow {
    text: String,
    attrs: CellAttributes,
    line: Option<Line>,
}

thread_local! {
    static MD_CACHE: std::cell::RefCell<HashMap<(String, usize), Vec<AgentRow>>> =
        std::cell::RefCell::new(HashMap::new());
}

impl AgentRow {
    fn rendered(line: Line) -> Self {
        AgentRow {
            text: String::new(),
            attrs: attr_default(),
            line: Some(line),
        }
    }
}

fn apply_sgr(attrs: &mut CellAttributes, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *attrs = CellAttributes::default(),
            1 => {
                attrs.set_intensity(Intensity::Bold);
            }
            2 => {
                attrs.set_intensity(Intensity::Half);
            }
            3 => {
                attrs.set_italic(true);
            }
            4 => {
                attrs.set_underline(Underline::Single);
            }
            7 => {
                attrs.set_reverse(true);
            }
            22 => {
                attrs.set_intensity(Intensity::Normal);
            }
            23 => {
                attrs.set_italic(false);
            }
            24 => {
                attrs.set_underline(Underline::None);
            }
            27 => {
                attrs.set_reverse(false);
            }
            30..=37 => {
                attrs.set_foreground(ColorAttribute::PaletteIndex((codes[i] - 30) as u8));
            }
            39 => {
                attrs.set_foreground(ColorAttribute::Default);
            }
            40..=47 => {
                attrs.set_background(ColorAttribute::PaletteIndex((codes[i] - 40) as u8));
            }
            49 => {
                attrs.set_background(ColorAttribute::Default);
            }
            90..=97 => {
                attrs.set_foreground(ColorAttribute::PaletteIndex((codes[i] - 90 + 8) as u8));
            }
            100..=107 => {
                attrs.set_background(ColorAttribute::PaletteIndex((codes[i] - 100 + 8) as u8));
            }
            38 | 48 => {
                let fg = codes[i] == 38;
                let color = if codes.get(i + 1) == Some(&5) {
                    let c =
                        ColorAttribute::PaletteIndex(codes.get(i + 2).copied().unwrap_or(0) as u8);
                    i += 2;
                    c
                } else if codes.get(i + 1) == Some(&2) {
                    let r = codes.get(i + 2).copied().unwrap_or(0) as u8;
                    let g = codes.get(i + 3).copied().unwrap_or(0) as u8;
                    let b = codes.get(i + 4).copied().unwrap_or(0) as u8;
                    i += 4;
                    ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple::from((r, g, b)))
                } else {
                    ColorAttribute::Default
                };
                if fg {
                    attrs.set_foreground(color);
                } else {
                    attrs.set_background(color);
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn ansi_line_to_line(s: &str, seqno: SequenceNo) -> Line {
    let mut attrs = CellAttributes::default();
    let mut cells: Vec<Cell> = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte = None;
                for pc in chars.by_ref() {
                    if pc.is_ascii_digit() || pc == ';' {
                        params.push(pc);
                    } else {
                        final_byte = Some(pc);
                        break;
                    }
                }
                if final_byte == Some('m') {
                    apply_sgr(&mut attrs, &params);
                }
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        cells.push(Cell::new(c, attrs.clone()));
    }
    Line::from_cells(cells, seqno)
}

fn markdown_to_lines(md: &str, cols: usize) -> Vec<AgentRow> {
    let trimmed = md.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let key = (trimmed.to_string(), cols);
    if let Some(cached) = MD_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let width = cols.max(20);
    let events = markdown::parse(trimmed);
    let ansi = markdown_terminal::render_with(&events, &markdown_terminal::Theme::default(), width);
    let mut result: Vec<AgentRow> = ansi
        .split('\n')
        .map(|raw| AgentRow::rendered(ansi_line_to_line(raw, 0)))
        .collect();
    while result.len() > 1
        && result
            .last()
            .and_then(|r| r.line.as_ref())
            .is_some_and(|line| line.is_whitespace())
    {
        result.pop();
    }
    MD_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key, result.clone());
    });
    result
}

fn blank_row() -> AgentRow {
    AgentRow {
        text: String::new(),
        attrs: attr_default(),
        line: None,
    }
}

fn truncate_to(text: &str, max: usize) -> String {
    let max = max.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let mut s: String = chars[..max.saturating_sub(1)].iter().collect();
    s.push('…');
    s
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
                line: None,
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
                line: None,
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
        "assistant_message" => rows.extend(markdown_to_lines(trimmed, cols)),
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
        line: None,
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

struct QuestionOption {
    label: String,
    description: Option<String>,
}

struct Question {
    prompt: String,
    header: String,
    options: Vec<QuestionOption>,
}

fn parse_questions(input: &Value) -> Vec<Question> {
    let Some(items) = input.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let prompt = item.get("question")?.as_str()?.to_string();
            let header = item.get("header")?.as_str()?.to_string();
            let options = item
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|opt| {
                            Some(QuestionOption {
                                label: opt.get("label")?.as_str()?.to_string(),
                                description: opt
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .filter(|d| !d.is_empty())
                                    .map(String::from),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Question {
                prompt,
                header,
                options,
            })
        })
        .collect()
}

fn pending_is_question(request: &PermissionRequest) -> bool {
    request.kind == "question"
        && request
            .input
            .as_ref()
            .map(|input| !parse_questions(input).is_empty())
            .unwrap_or(false)
}

fn build_question_response(
    request: &PermissionRequest,
    answer: String,
) -> Option<PermissionResponse> {
    let input = request.input.clone().unwrap_or_default();
    let header = parse_questions(&input).into_iter().next()?.header;
    let mut answers = serde_json::Map::new();
    answers.insert(header, Value::from(answer));
    let mut updated = input.as_object().cloned().unwrap_or_default();
    updated.insert("answers".to_string(), Value::Object(answers));
    Some(PermissionResponse::Allow {
        selected_action_id: None,
        updated_input: Some(Value::Object(updated)),
    })
}

fn question_rows(request: &PermissionRequest, cols: usize) -> Vec<AgentRow> {
    let mut rows = vec![blank_row()];
    let questions = request
        .input
        .as_ref()
        .map(parse_questions)
        .unwrap_or_default();
    for question in &questions {
        push_wrapped(
            &mut rows,
            "⚠ ",
            &question.prompt,
            &attr_bold_fg(AnsiColor::Yellow),
            cols,
        );
        for (i, option) in question.options.iter().enumerate().take(9) {
            push_wrapped(
                &mut rows,
                "  ",
                &format!("[{}] {}", i + 1, option.label),
                &attr_fg(AnsiColor::Yellow),
                cols,
            );
            if let Some(description) = &option.description {
                push_wrapped(&mut rows, "      ", description, &attr_dim(), cols);
            }
        }
    }
    rows
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Scroll,
    Compose,
}

enum PickerAction {
    OpenAgent(String),
    NewAgentInWorkspace(String),
    WorktreeInPlace(String),
    WorktreeNewBranch(String),
    WorktreeCheckoutBranch(String),
    SearchDirectory,
    NewDirectory,
    CloneRepo,
    ChooseDomain(String),
    AddConnection,
    SelectProvider(String),
}

#[derive(Clone)]
enum PendingCreate {
    Workspace(String),
    WorktreeBranchOff { cwd: String, base_branch: String },
    WorktreeCheckout { cwd: String, ref_name: String },
    NewDirectory(String),
    CloneRepo(String),
}

#[derive(Clone, Copy, PartialEq)]
enum WorktreeAction {
    BranchOff,
    Checkout,
}

#[derive(Clone)]
enum WizardStep {
    Directory,
    Method {
        cwd: String,
    },
    Branch {
        cwd: String,
        action: WorktreeAction,
    },
    Provider {
        pending: PendingCreate,
        providers: Vec<String>,
    },
}

#[derive(Clone)]
struct WizardFrame {
    step: WizardStep,
    crumbs: Vec<String>,
}

fn crumb_basename(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

struct PickerEntry {
    label: String,
    action: PickerAction,
}

struct PickerGroup {
    label: String,
    collapsed: bool,
    entries: Vec<PickerEntry>,
}

enum PickerRow {
    Header(usize),
    Entry(usize, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum InputKind {
    SearchDirectory,
    BranchOff,
    CheckoutBranch,
    NewDirectory,
    CloneRepo,
    AddConnection,
}

impl InputKind {
    fn autocompletes(self) -> bool {
        matches!(
            self,
            InputKind::SearchDirectory | InputKind::BranchOff | InputKind::CheckoutBranch
        )
    }

    fn is_branch(self) -> bool {
        matches!(self, InputKind::BranchOff | InputKind::CheckoutBranch)
    }
}

enum PickerStage {
    Browse,
    Input {
        kind: InputKind,
        label: String,
        buffer: String,
        context: Option<String>,
        suggestions: Vec<String>,
        suggestion_selected: usize,
        suggest_gen: u64,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum PickerKind {
    Hub,
    Connections,
    Providers,
}

struct PickerState {
    title: String,
    kind: PickerKind,
    groups: Vec<PickerGroup>,
    selected: usize,
    stage: PickerStage,
    pending: Option<PendingCreate>,
    pending_delete: bool,
    crumbs: Vec<String>,
}

impl PickerState {
    fn visible_rows(&self) -> Vec<PickerRow> {
        let mut rows = Vec::new();
        for (gi, group) in self.groups.iter().enumerate() {
            rows.push(PickerRow::Header(gi));
            if !group.collapsed {
                for ei in 0..group.entries.len() {
                    rows.push(PickerRow::Entry(gi, ei));
                }
            }
        }
        rows
    }
}

fn is_active_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "working" | "thinking" | "streaming" | "in_progress" | "busy" | "active"
    )
}

fn status_glyph(agent: &AgentSnapshot) -> &'static str {
    if is_active_status(&agent.status) {
        "●"
    } else if agent.requires_attention {
        "⚠"
    } else {
        "○"
    }
}

fn session_title(agent: &AgentSnapshot) -> String {
    match agent.title.clone().filter(|t| !t.trim().is_empty()) {
        Some(title) => title.replace('\n', " ").trim().chars().take(80).collect(),
        None => "New session".to_string(),
    }
}

fn session_name(workspace_name: Option<&str>, agent: &AgentSnapshot) -> String {
    workspace_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(80).collect())
        .unwrap_or_else(|| session_title(agent))
}

fn agent_row_label(
    agent: &AgentSnapshot,
    workspace_name: Option<&str>,
    context: Option<&str>,
) -> String {
    let primary = session_name(workspace_name, agent);
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!("{} {}  ·  {}", status_glyph(agent), primary, context),
        None => format!("{} {}", status_glyph(agent), primary),
    }
}

fn workspace_context(ws: &Workspace) -> Option<String> {
    if let Some(branch) = ws.branch() {
        return Some(branch.to_string());
    }
    if ws.workspace_kind == "worktree" {
        let slug = basename(ws.cwd());
        if !slug.is_empty() {
            return Some(slug.to_string());
        }
    }
    None
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

fn provider_display(id: &str) -> String {
    match id {
        "claude-code" | "claude_code" | "claudecode" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "copilot" | "github-copilot" => "GitHub Copilot".to_string(),
        "opencode" => "OpenCode".to_string(),
        "pi" => "Pi".to_string(),
        other => other.to_string(),
    }
}

fn first_entry_row(groups: &[PickerGroup]) -> usize {
    let mut row = 0;
    for group in groups {
        row += 1;
        if !group.collapsed {
            if !group.entries.is_empty() {
                return row;
            }
            row += group.entries.len();
        }
    }
    0
}

fn build_connection_groups() -> Vec<PickerGroup> {
    let mux = Mux::get();
    let mut daemons = Vec::new();
    for domain in mux.iter_domains() {
        if domain.downcast_ref::<paseo_mux::PaseoDomain>().is_some() {
            let name = domain.domain_name().to_string();
            daemons.push(PickerEntry {
                label: format!("connect  {name}"),
                action: PickerAction::ChooseDomain(name),
            });
        }
    }
    vec![
        PickerGroup {
            label: "Connections".to_string(),
            collapsed: false,
            entries: daemons,
        },
        PickerGroup {
            label: "New".to_string(),
            collapsed: false,
            entries: vec![PickerEntry {
                label: "add connection  (relay URL or host:port)".to_string(),
                action: PickerAction::AddConnection,
            }],
        },
    ]
}

fn parse_connect_target(input: &str) -> Option<(String, paseo_mux::ConnectTarget)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input.starts_with("http://") || input.starts_with("https://") {
        Some((
            "paseo:relay".to_string(),
            paseo_mux::ConnectTarget::Relay {
                offer_url: input.to_string(),
            },
        ))
    } else if input.contains(':') {
        Some((
            format!("paseo:{input}"),
            paseo_mux::ConnectTarget::Local {
                host_port: input.to_string(),
                use_tls: false,
                password: None,
            },
        ))
    } else {
        None
    }
}

fn agent_rank(agent: &AgentSnapshot) -> (u8, u8) {
    let attention = if agent.requires_attention { 0 } else { 1 };
    let activity = if is_active_status(&agent.status) {
        0
    } else if matches!(
        agent.status.as_str(),
        "waiting" | "paused" | "blocked" | "input_required"
    ) {
        1
    } else {
        2
    };
    (attention, activity)
}

fn build_picker_groups(
    agents: Vec<AgentListEntry>,
    workspaces: Vec<Workspace>,
) -> Vec<PickerGroup> {
    let mut groups = vec![PickerGroup {
        label: "Create".to_string(),
        collapsed: false,
        entries: vec![
            PickerEntry {
                label: "search for a directory + agent".to_string(),
                action: PickerAction::SearchDirectory,
            },
            PickerEntry {
                label: "new directory + agent".to_string(),
                action: PickerAction::NewDirectory,
            },
            PickerEntry {
                label: "clone GitHub repo + agent".to_string(),
                action: PickerAction::CloneRepo,
            },
        ],
    }];

    struct Unit {
        rank: (u8, u8),
        sort_key: String,
        entries: Vec<PickerEntry>,
    }
    struct Proj {
        display: String,
        units: Vec<Unit>,
    }

    let ws_ids: HashSet<&str> = workspaces.iter().map(|w| w.id.as_str()).collect();
    let mut agents_by_ws: HashMap<String, Vec<AgentSnapshot>> = HashMap::new();
    let mut orphans: Vec<AgentSnapshot> = Vec::new();
    for entry in agents.into_iter().filter(|e| e.agent.archived_at.is_none()) {
        let agent = entry.agent;
        match agent
            .workspace_id
            .as_ref()
            .filter(|id| ws_ids.contains(id.as_str()))
        {
            Some(id) => agents_by_ws.entry(id.clone()).or_default().push(agent),
            None => orphans.push(agent),
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut index: HashMap<String, Proj> = HashMap::new();
    let other_key = "\u{0}other".to_string();

    for ws in &workspaces {
        let key = if ws.project_id.is_empty() {
            ws.project_display_name.clone()
        } else {
            ws.project_id.clone()
        };
        if key.is_empty() {
            continue;
        }
        let display = if ws.project_display_name.is_empty() {
            key.clone()
        } else {
            ws.project_display_name.clone()
        };
        index.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Proj {
                display,
                units: Vec::new(),
            }
        });
        let name = if ws.name.trim().is_empty() {
            basename(ws.cwd()).to_string()
        } else {
            ws.name.clone()
        };
        let context = workspace_context(ws);

        let unit = match agents_by_ws.remove(&ws.id).filter(|v| !v.is_empty()) {
            Some(mut ws_agents) => {
                ws_agents.sort_by(|a, b| {
                    agent_rank(a)
                        .cmp(&agent_rank(b))
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                });
                let primary = &ws_agents[0];
                let extra = ws_agents.len() - 1;
                let base = agent_row_label(primary, Some(&name), context.as_deref());
                let main_label = if extra > 0 {
                    format!("{base}   (+{extra})")
                } else {
                    base
                };
                let mut entries = vec![PickerEntry {
                    label: main_label,
                    action: PickerAction::OpenAgent(primary.id.clone()),
                }];
                for agent in &ws_agents[1..] {
                    entries.push(PickerEntry {
                        label: format!("  {} {}", status_glyph(agent), session_title(agent)),
                        action: PickerAction::OpenAgent(agent.id.clone()),
                    });
                }
                entries.push(PickerEntry {
                    label: "  ⊕ new agent".to_string(),
                    action: PickerAction::NewAgentInWorkspace(ws.cwd().to_string()),
                });
                Unit {
                    rank: agent_rank(primary),
                    sort_key: name.to_lowercase(),
                    entries,
                }
            }
            None => {
                let label = match &context {
                    Some(context) => format!("⊕ {}  ·  {}", name, context),
                    None => format!("⊕ {}", name),
                };
                Unit {
                    rank: (9, 9),
                    sort_key: name.to_lowercase(),
                    entries: vec![PickerEntry {
                        label,
                        action: PickerAction::NewAgentInWorkspace(ws.cwd().to_string()),
                    }],
                }
            }
        };
        if let Some(proj) = index.get_mut(&key) {
            proj.units.push(unit);
        }
    }

    for agent in orphans {
        index.entry(other_key.clone()).or_insert_with(|| {
            order.push(other_key.clone());
            Proj {
                display: "Other".to_string(),
                units: Vec::new(),
            }
        });
        if let Some(proj) = index.get_mut(&other_key) {
            proj.units.push(Unit {
                rank: agent_rank(&agent),
                sort_key: session_title(&agent).to_lowercase(),
                entries: vec![PickerEntry {
                    label: agent_row_label(&agent, None, None),
                    action: PickerAction::OpenAgent(agent.id.clone()),
                }],
            });
        }
    }

    for proj in index.values_mut() {
        proj.units.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then_with(|| a.sort_key.cmp(&b.sort_key))
        });
    }

    let project_rank = |key: &str| -> (u8, u8) {
        index
            .get(key)
            .and_then(|p| p.units.first().map(|u| u.rank))
            .unwrap_or((9, 9))
    };

    order.sort_by(|x, y| match (*x == other_key, *y == other_key) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => project_rank(x).cmp(&project_rank(y)).then_with(|| {
            let dx = index
                .get(x)
                .map(|p| p.display.to_lowercase())
                .unwrap_or_default();
            let dy = index
                .get(y)
                .map(|p| p.display.to_lowercase())
                .unwrap_or_default();
            dx.cmp(&dy)
        }),
    });

    for key in order {
        if let Some(proj) = index.remove(&key) {
            let entries: Vec<PickerEntry> =
                proj.units.into_iter().flat_map(|u| u.entries).collect();
            groups.push(PickerGroup {
                label: proj.display,
                collapsed: true,
                entries,
            });
        }
    }

    groups
}

struct AgentState {
    title: String,
    status_message: Option<String>,
    items: Vec<TimelineItem>,
    pending: Option<PermissionRequest>,
    picker: Option<PickerState>,
    cwd: String,
    workspace_name: Option<String>,
    mode: Mode,
    composer: String,
    composer_cursor: usize,
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
    create_stack: Vec<WizardFrame>,
}

impl AgentState {
    fn composer_char_len(&self) -> usize {
        self.composer.chars().count()
    }

    fn composer_byte_offset(&self, char_idx: usize) -> usize {
        self.composer
            .char_indices()
            .nth(char_idx)
            .map_or(self.composer.len(), |(byte, _)| byte)
    }

    fn composer_insert(&mut self, c: char) {
        let cursor = self.composer_cursor.min(self.composer_char_len());
        let offset = self.composer_byte_offset(cursor);
        self.composer.insert(offset, c);
        self.composer_cursor = cursor + 1;
    }

    fn composer_backspace(&mut self) {
        let cursor = self.composer_cursor.min(self.composer_char_len());
        if cursor == 0 {
            return;
        }
        let start = self.composer_byte_offset(cursor - 1);
        let end = self.composer_byte_offset(cursor);
        self.composer.replace_range(start..end, "");
        self.composer_cursor = cursor - 1;
    }

    fn composer_line_col(&self) -> (usize, usize) {
        let cursor = self.composer_cursor.min(self.composer_char_len());
        let mut line = 0;
        let mut col = 0;
        for ch in self.composer.chars().take(cursor) {
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn composer_set_line_col(&mut self, line: usize, col: usize) {
        let lines: Vec<&str> = self.composer.split('\n').collect();
        let line = line.min(lines.len().saturating_sub(1));
        let target_col = col.min(lines[line].chars().count());
        let mut idx = 0;
        for l in &lines[..line] {
            idx += l.chars().count() + 1;
        }
        self.composer_cursor = idx + target_col;
    }

    fn composer_move_horizontal(&mut self, delta: isize) {
        let len = self.composer_char_len();
        let next = (self.composer_cursor.min(len) as isize + delta).clamp(0, len as isize);
        self.composer_cursor = next as usize;
    }

    fn composer_move_vertical(&mut self, delta: isize) {
        let (line, col) = self.composer_line_col();
        let next = (line as isize + delta).max(0) as usize;
        self.composer_set_line_col(next, col);
    }

    fn composer_home(&mut self) {
        let (line, _) = self.composer_line_col();
        self.composer_set_line_col(line, 0);
    }

    fn composer_end(&mut self) {
        let (line, _) = self.composer_line_col();
        self.composer_set_line_col(line, usize::MAX);
    }

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
            if let PickerStage::Input {
                kind,
                label,
                buffer,
                suggestions,
                suggestion_selected,
                ..
            } = &picker.stage
            {
                let mut transcript = Vec::new();
                push_wrapped(
                    &mut transcript,
                    "",
                    label,
                    &attr_bold_fg(AnsiColor::Teal),
                    cols,
                );
                if !picker.crumbs.is_empty() {
                    transcript.push(AgentRow {
                        text: truncate_to(&picker.crumbs.join("  ›  "), cols),
                        attrs: attr_dim(),
                        line: None,
                    });
                    transcript.push(blank_row());
                }
                for (i, suggestion) in suggestions.iter().enumerate() {
                    let selected = i == *suggestion_selected;
                    let marker = if selected { "❯ " } else { "  " };
                    let attrs = if selected {
                        attr_bold_fg(AnsiColor::Teal)
                    } else {
                        attr_default()
                    };
                    transcript.push(AgentRow {
                        text: truncate_to(&format!("{marker}{suggestion}"), cols),
                        attrs,
                        line: None,
                    });
                }
                let hint = if kind.autocompletes() {
                    "type to search · ↑/↓ pick · Enter select · Esc back"
                } else {
                    "Enter to confirm · Esc back"
                };
                self.transcript = transcript;
                self.footer = vec![
                    AgentRow {
                        text: format!("❯ {buffer}"),
                        attrs: attr_default(),
                        line: None,
                    },
                    AgentRow {
                        text: hint.to_string(),
                        attrs: attr_dim(),
                        line: None,
                    },
                ];
                self.clamp_scroll();
                return;
            }

            let selected = picker.selected;
            let rows = picker.visible_rows();
            let count = rows.len();
            let mut transcript = Vec::new();
            transcript.push(AgentRow {
                text: picker.title.clone(),
                attrs: attr_bold_fg(AnsiColor::Teal),
                line: None,
            });
            if !picker.crumbs.is_empty() {
                transcript.push(AgentRow {
                    text: truncate_to(&picker.crumbs.join("  ›  "), cols),
                    attrs: attr_dim(),
                    line: None,
                });
            }
            transcript.push(blank_row());
            if picker.groups.is_empty() {
                push_wrapped(&mut transcript, "  ", "nothing here", &attr_dim(), cols);
            }
            let mut sel_row = transcript.len();
            for (i, row) in rows.iter().enumerate() {
                let active = i == selected;
                if active {
                    sel_row = transcript.len();
                }
                let (prefix, label, attrs) = match row {
                    PickerRow::Header(gi) => {
                        let group = &picker.groups[*gi];
                        let glyph = if group.collapsed { "▸" } else { "▾" };
                        let marker = if active { "❯ " } else { "  " };
                        (
                            format!("{marker}{glyph} "),
                            format!("{}  ({})", group.label, group.entries.len()),
                            attr_bold_fg(AnsiColor::Teal),
                        )
                    }
                    PickerRow::Entry(gi, ei) => {
                        let entry = &picker.groups[*gi].entries[*ei];
                        let marker = if active { "❯   " } else { "    " };
                        let attrs = if active {
                            attr_bold_fg(AnsiColor::Silver)
                        } else {
                            attr_default()
                        };
                        (marker.to_string(), entry.label.clone(), attrs)
                    }
                };
                let budget = cols.saturating_sub(prefix.chars().count());
                transcript.push(AgentRow {
                    text: format!("{prefix}{}", truncate_to(&label, budget)),
                    attrs,
                    line: None,
                });
            }
            self.transcript = transcript;
            self.footer = vec![if picker.pending_delete {
                AgentRow {
                    text: "d again: archive agent  ·  any other key: cancel".to_string(),
                    attrs: attr_fg(AnsiColor::Yellow),
                    line: None,
                }
            } else {
                let tail = if picker.crumbs.is_empty() {
                    "dd:archive · o:fold · r:refresh · q: close"
                } else {
                    "Enter select · Esc back"
                };
                AgentRow {
                    text: format!(
                        "❯ {}/{}  ·  j/k · {tail}",
                        (selected + 1).min(count.max(1)),
                        count
                    ),
                    attrs: attr_dim(),
                    line: None,
                }
            }];

            let view = self.view_rows().max(1);
            if sel_row < self.scroll {
                self.scroll = sel_row;
            } else if sel_row >= self.scroll + view {
                self.scroll = sel_row + 1 - view;
            }
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
        let question_pending = self.pending.as_ref().is_some_and(pending_is_question);
        if let Some(request) = self.pending.as_ref().filter(|r| pending_is_question(r)) {
            transcript.extend(question_rows(request, cols));
        }
        self.transcript = transcript;

        let mut footer = Vec::new();
        if let Some(request) = &self.pending {
            if question_pending {
                footer.push(AgentRow {
                    text: "⚠ question — [1-9]: pick · i: type your own · n/Esc: dismiss"
                        .to_string(),
                    attrs: attr_fg(AnsiColor::Yellow),
                    line: None,
                });
            } else {
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
                if let Some(description) =
                    request.description.clone().filter(|d| !d.trim().is_empty())
                {
                    push_wrapped(
                        &mut footer,
                        "  ",
                        &truncate_to(
                            description.replace('\n', " ").trim(),
                            cols.saturating_sub(2),
                        ),
                        &attr_dim(),
                        cols,
                    );
                }
                let choices = if request.actions.is_empty() {
                    "[y] allow   [n] deny".to_string()
                } else {
                    request
                        .actions
                        .iter()
                        .take(9)
                        .enumerate()
                        .map(|(i, action)| format!("[{}] {}", i + 1, action.label))
                        .collect::<Vec<_>>()
                        .join("   ")
                };
                push_wrapped(
                    &mut footer,
                    "  ",
                    &choices,
                    &attr_fg(AnsiColor::Yellow),
                    cols,
                );
            }
        }
        if !self.agent_status.is_empty() || self.model.is_some() {
            let model = self.model.clone().unwrap_or_else(|| "—".to_string());
            let location = match self.workspace_name.as_deref() {
                Some(ws) if !ws.is_empty() && !self.cwd.is_empty() => {
                    format!("{ws}  ·  {}", self.cwd)
                }
                Some(ws) if !ws.is_empty() => ws.to_string(),
                _ => self.cwd.clone(),
            };
            if !location.is_empty() {
                footer.push(AgentRow {
                    text: truncate_to(&location, cols),
                    attrs: attr_dim(),
                    line: None,
                });
            }
            footer.push(AgentRow {
                text: format!(
                    "[{}]  {}  ·  model:{}  ·  mode:{}  ·  effort:{}",
                    self.agent_status,
                    provider_display(&self.provider),
                    model,
                    self.mode_label(),
                    self.effort_label()
                ),
                attrs: attr_fg(AnsiColor::Teal),
                line: None,
            });
            footer.push(AgentRow {
                text: "d:diff  t:terminal  ·  m:mode  M:model  e:effort  x:stop".to_string(),
                attrs: attr_dim(),
                line: None,
            });
        }
        match self.mode {
            Mode::Compose => {
                footer.push(AgentRow {
                    text: "Enter: send  ·  Shift-Enter: newline  ·  ←→↑↓: move  ·  Esc: cancel"
                        .to_string(),
                    attrs: attr_dim(),
                    line: None,
                });
                for (i, line) in self.composer.split('\n').enumerate() {
                    let prefix = if i == 0 { "❯ " } else { "  " };
                    footer.push(AgentRow {
                        text: format!("{prefix}{line}"),
                        attrs: attr_default(),
                        line: None,
                    });
                }
            }
            Mode::Scroll => footer.push(AgentRow {
                text: "❯ (i: type · j/k · Ctrl-d/u · g/G · d: review · t: terminal · q/Esc: close)"
                    .to_string(),
                attrs: attr_dim(),
                line: None,
            }),
        }
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

    fn row_line(&self, screen_row: usize) -> Line {
        let cols = self.size.cols;
        let view_rows = self.view_rows();
        let row = if screen_row < view_rows {
            self.transcript.get(self.scroll + screen_row)
        } else {
            self.footer.get(screen_row - view_rows)
        };
        match row {
            Some(AgentRow {
                line: Some(line), ..
            }) => line.clone(),
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

    fn arc(&self) -> Option<Arc<PaseoAgentPane>> {
        let weak = self.weak.lock().clone();
        weak.upgrade()
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
            state.cwd = snapshot.cwd.clone();
            state.title = session_name(state.workspace_name.as_deref(), snapshot);
            state.provider = snapshot.provider.clone();
            state.agent_status = snapshot.status.clone();
            state.model = snapshot.model.clone();
            state.current_mode_id = snapshot.current_mode_id.clone();
            state.available_modes = snapshot.available_modes.clone();
            state.thinking_option_id = snapshot.thinking_option_id.clone();
            state.requires_attention = snapshot.requires_attention;
            if state.pending.is_none() {
                state.pending = snapshot.pending_permissions.first().cloned();
            }
            state.rebuild_rows();
        });
    }

    fn set_models(&self, models: Vec<ModelDefinition>) {
        self.mutate(|state| {
            state.models = models;
            state.rebuild_rows();
        });
    }

    fn set_workspace_name(&self, name: String) {
        if name.trim().is_empty() {
            return;
        }
        self.mutate(|state| {
            state.title = name.clone();
            state.workspace_name = Some(name);
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

    fn open_review(&self) {
        self.window
            .notify(TermWindowNotif::Apply(Box::new(|term_window| {
                let args = config::keyassignment::ReviewPaneArgs::default();
                if let Err(err) = crate::review::open_review_pane(term_window, &args) {
                    log::error!("failed to open review pane: {err:#}");
                }
            })));
    }

    fn open_terminal(&self) {
        let (cwd, size) = {
            let state = self.state.lock();
            (state.cwd.clone(), state.size)
        };
        if cwd.is_empty() {
            return;
        }
        let Some((_, window_id, _)) = Mux::get().resolve_pane_id(self.pane_id) else {
            return;
        };
        let domain = self.domain.clone();
        promise::spawn::spawn(async move {
            if let Err(err) = domain.spawn(size, None, Some(cwd), window_id).await {
                log::error!("paseo: failed to open terminal: {err:#}");
            }
        })
        .detach();
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
            state.composer_cursor = 0;
            std::mem::take(&mut state.composer).trim().to_string()
        };
        if text.is_empty() {
            self.mutate(|state| state.rebuild_rows());
            return;
        }

        let question_response = {
            let state = self.state.lock();
            state
                .pending
                .as_ref()
                .filter(|r| pending_is_question(r))
                .and_then(|request| {
                    build_question_response(request, text.clone())
                        .map(|response| (request.id.clone(), response))
                })
        };

        if let Some((request_id, response)) = question_response {
            if let (Some(agent_id), Some(client)) = (self.agent_id.lock().clone(), self.client()) {
                promise::spawn::spawn(async move {
                    let _ = client
                        .respond_permission(&agent_id, &request_id, response)
                        .await;
                })
                .detach();
            }
            self.mutate(|state| {
                state.pending = None;
                state.mode = Mode::Scroll;
                state.rebuild_rows();
            });
            return;
        }

        if let (Some(agent_id), Some(client)) = (self.agent_id.lock().clone(), self.client()) {
            promise::spawn::spawn(async move {
                let _ = client.send_agent_message(&agent_id, &text).await;
            })
            .detach();
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
                    updated_input: None,
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

    fn respond_action(&self, index: usize) {
        let (agent_id, request_id, response) = {
            let state = self.state.lock();
            let Some(request) = &state.pending else {
                return;
            };
            let response = if pending_is_question(request) {
                let input = request.input.clone().unwrap_or_default();
                let label = parse_questions(&input)
                    .into_iter()
                    .next()
                    .and_then(|q| q.options.into_iter().nth(index))
                    .map(|option| option.label);
                let Some(response) =
                    label.and_then(|label| build_question_response(request, label))
                else {
                    return;
                };
                response
            } else {
                let Some(action) = request.actions.get(index) else {
                    return;
                };
                if action.behavior == "deny" {
                    PermissionResponse::Deny {
                        message: None,
                        interrupt: false,
                    }
                } else {
                    PermissionResponse::Allow {
                        selected_action_id: Some(action.id.clone()),
                        updated_input: None,
                    }
                }
            };
            (self.agent_id.lock().clone(), request.id.clone(), response)
        };
        if let (Some(agent_id), Some(client)) = (agent_id, self.client()) {
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
                let agents = client.fetch_agents().await.unwrap_or_default();
                let workspaces = client.fetch_workspaces().await.unwrap_or_default();
                let groups = build_picker_groups(agents, workspaces);
                if let Some(pane) = weak.upgrade() {
                    pane.enter_hub(
                        "Paseo — open an agent, or start one in a project".to_string(),
                        groups,
                    );
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
        let workspace_id = snapshot.workspace_id.clone();
        *self.agent_id.lock() = Some(agent_id.clone());
        self.mutate(|state| {
            state.picker = None;
            state.follow = true;
            state.scroll = 0;
        });
        self.set_snapshot(&snapshot);

        let weak = Arc::downgrade(self);
        let Some(client) = self.client() else {
            return;
        };
        promise::spawn::spawn(async move {
            let workspaces = client.fetch_workspaces().await.unwrap_or_default();
            let name = workspaces
                .iter()
                .find(|ws| {
                    workspace_id.as_deref() == Some(ws.id.as_str()) || ws.cwd() == cwd.as_str()
                })
                .map(|ws| ws.name.clone());
            if let (Some(name), Some(pane)) = (name, weak.upgrade()) {
                pane.set_workspace_name(name);
            }

            if !provider.is_empty() {
                if let Ok(models) = client.list_provider_models(&provider, Some(&cwd)).await {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_models(models);
                    }
                }
            }

            let _ = client
                .set_timeline_subscription(std::slice::from_ref(&agent_id))
                .await;

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

    fn start_chooser(self: &Arc<Self>) {
        self.mutate(|state| {
            state.title = "Paseo — connect".to_string();
        });
        let groups = build_connection_groups();
        let selected = first_entry_row(&groups);
        self.enter_picker(
            "Paseo — connect to a daemon".to_string(),
            PickerKind::Connections,
            groups,
            None,
            selected,
        );
    }

    fn enter_hub(&self, title: String, groups: Vec<PickerGroup>) {
        self.enter_picker(title, PickerKind::Hub, groups, None, 0);
    }

    fn enter_picker(
        &self,
        title: String,
        kind: PickerKind,
        groups: Vec<PickerGroup>,
        pending: Option<PendingCreate>,
        selected: usize,
    ) {
        self.enter_picker_crumbs(title, kind, groups, pending, selected, Vec::new());
    }

    fn enter_picker_crumbs(
        &self,
        title: String,
        kind: PickerKind,
        groups: Vec<PickerGroup>,
        pending: Option<PendingCreate>,
        selected: usize,
        crumbs: Vec<String>,
    ) {
        self.mutate(|state| {
            state.status_message = None;
            state.follow = false;
            state.scroll = 0;
            state.picker = Some(PickerState {
                title,
                kind,
                groups,
                selected,
                stage: PickerStage::Browse,
                pending,
                pending_delete: false,
                crumbs,
            });
            state.rebuild_rows();
        });
    }

    fn begin_create(self: &Arc<Self>, pending: PendingCreate) {
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.status_message = Some("⟳ checking available agents…".to_string());
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let providers = client.list_available_providers().await.unwrap_or_default();
            let Some(pane) = weak.upgrade() else {
                return;
            };
            match providers.len() {
                0 => pane.set_status(
                    "Error".into(),
                    Some("no agent providers available on this daemon".into()),
                ),
                1 => pane.execute_create(pending, providers[0].clone()),
                _ => {
                    let entries = providers
                        .into_iter()
                        .map(|id| PickerEntry {
                            label: provider_display(&id),
                            action: PickerAction::SelectProvider(id),
                        })
                        .collect();
                    let groups = vec![PickerGroup {
                        label: "Providers".to_string(),
                        collapsed: false,
                        entries,
                    }];
                    let selected = first_entry_row(&groups);
                    pane.enter_picker(
                        "Choose the agent to run".to_string(),
                        PickerKind::Providers,
                        groups,
                        Some(pending),
                        selected,
                    );
                }
            }
        })
        .detach();
    }

    fn execute_create(self: &Arc<Self>, pending: PendingCreate, provider: String) {
        match pending {
            PendingCreate::Workspace(cwd) => self.create_agent_with(cwd, provider),
            PendingCreate::WorktreeBranchOff { cwd, base_branch } => {
                self.create_worktree_agent(cwd, "branch-off", None, Some(base_branch), provider)
            }
            PendingCreate::WorktreeCheckout { cwd, ref_name } => {
                self.create_worktree_agent(cwd, "checkout", Some(ref_name), None, provider)
            }
            PendingCreate::NewDirectory(value) => {
                self.run_project_creation(InputKind::NewDirectory, value, provider)
            }
            PendingCreate::CloneRepo(value) => {
                self.run_project_creation(InputKind::CloneRepo, value, provider)
            }
        }
    }

    fn wizard_active(&self) -> bool {
        !self.state.lock().create_stack.is_empty()
    }

    fn wizard_top_crumbs(&self) -> Vec<String> {
        self.state
            .lock()
            .create_stack
            .last()
            .map(|frame| frame.crumbs.clone())
            .unwrap_or_default()
    }

    fn wizard_reset(&self) {
        self.state.lock().create_stack.clear();
    }

    fn wizard_start(self: &Arc<Self>, step: WizardStep, crumbs: Vec<String>) {
        self.wizard_reset();
        self.wizard_push(step, crumbs);
    }

    fn wizard_push(self: &Arc<Self>, step: WizardStep, crumbs: Vec<String>) {
        self.state
            .lock()
            .create_stack
            .push(WizardFrame { step, crumbs });
        self.wizard_render();
    }

    fn wizard_back(self: &Arc<Self>) {
        let empty = {
            let mut state = self.state.lock();
            state.create_stack.pop();
            state.create_stack.is_empty()
        };
        if empty {
            self.open_hub();
        } else {
            self.wizard_render();
        }
    }

    fn wizard_render(self: &Arc<Self>) {
        let frame = self.state.lock().create_stack.last().cloned();
        let Some(frame) = frame else {
            return;
        };
        match frame.step {
            WizardStep::Directory => self.show_directory_input(frame.crumbs),
            WizardStep::Method { cwd } => self.show_method_picker(cwd, frame.crumbs),
            WizardStep::Branch { cwd, action } => self.show_branch_input(cwd, action, frame.crumbs),
            WizardStep::Provider { pending, providers } => {
                self.show_provider_picker(pending, providers, frame.crumbs)
            }
        }
    }

    fn show_directory_input(self: &Arc<Self>, crumbs: Vec<String>) {
        self.set_input_stage(
            InputKind::SearchDirectory,
            "Search for a directory on the daemon:".to_string(),
            "~/".to_string(),
            None,
            crumbs,
        );
        self.refresh_suggestions();
    }

    fn show_branch_input(
        self: &Arc<Self>,
        cwd: String,
        action: WorktreeAction,
        crumbs: Vec<String>,
    ) {
        let (kind, label) = match action {
            WorktreeAction::BranchOff => {
                (InputKind::BranchOff, "Base branch for the new worktree:")
            }
            WorktreeAction::Checkout => (
                InputKind::CheckoutBranch,
                "Branch to check out in a worktree:",
            ),
        };
        self.set_input_stage(kind, label.to_string(), String::new(), Some(cwd), crumbs);
        self.refresh_suggestions();
    }

    fn show_method_picker(&self, cwd: String, crumbs: Vec<String>) {
        let groups = vec![PickerGroup {
            label: "New agent".to_string(),
            collapsed: false,
            entries: vec![
                PickerEntry {
                    label: "in this checkout".to_string(),
                    action: PickerAction::WorktreeInPlace(cwd.clone()),
                },
                PickerEntry {
                    label: "new branch in a worktree".to_string(),
                    action: PickerAction::WorktreeNewBranch(cwd.clone()),
                },
                PickerEntry {
                    label: "existing branch in a worktree".to_string(),
                    action: PickerAction::WorktreeCheckoutBranch(cwd),
                },
            ],
        }];
        let selected = first_entry_row(&groups);
        self.enter_picker_crumbs(
            "How should this agent run?".to_string(),
            PickerKind::Providers,
            groups,
            None,
            selected,
            crumbs,
        );
    }

    fn show_provider_picker(
        &self,
        pending: PendingCreate,
        providers: Vec<String>,
        crumbs: Vec<String>,
    ) {
        let entries = providers
            .into_iter()
            .map(|id| PickerEntry {
                label: provider_display(&id),
                action: PickerAction::SelectProvider(id),
            })
            .collect();
        let groups = vec![PickerGroup {
            label: "Providers".to_string(),
            collapsed: false,
            entries,
        }];
        let selected = first_entry_row(&groups);
        self.enter_picker_crumbs(
            "Choose the agent to run".to_string(),
            PickerKind::Providers,
            groups,
            Some(pending),
            selected,
            crumbs,
        );
    }

    fn set_input_stage(
        &self,
        kind: InputKind,
        label: String,
        buffer: String,
        context: Option<String>,
        crumbs: Vec<String>,
    ) {
        self.mutate(|state| {
            state.status_message = None;
            state.follow = false;
            state.scroll = 0;
            state.picker = Some(PickerState {
                title: label.clone(),
                kind: PickerKind::Providers,
                groups: Vec::new(),
                selected: 0,
                stage: PickerStage::Input {
                    kind,
                    label,
                    buffer,
                    context,
                    suggestions: Vec::new(),
                    suggestion_selected: 0,
                    suggest_gen: 0,
                },
                pending: None,
                pending_delete: false,
                crumbs,
            });
            state.rebuild_rows();
        });
    }

    fn wizard_to_provider(self: &Arc<Self>, pending: PendingCreate, crumbs: Vec<String>) {
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.status_message = Some("⟳ checking available agents…".to_string());
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let providers = client.list_available_providers().await.unwrap_or_default();
            let Some(pane) = weak.upgrade() else {
                return;
            };
            match providers.len() {
                0 => pane.set_status(
                    "Error".into(),
                    Some("no agent providers available on this daemon".into()),
                ),
                1 => {
                    pane.wizard_reset();
                    pane.execute_create(pending, providers[0].clone());
                }
                _ => pane.wizard_push(WizardStep::Provider { pending, providers }, crumbs),
            }
        })
        .detach();
    }

    fn open_hub(self: &Arc<Self>) {
        let Some(client) = self.client() else {
            return;
        };
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let agents = client.fetch_agents().await.unwrap_or_default();
            let workspaces = client.fetch_workspaces().await.unwrap_or_default();
            let groups = build_picker_groups(agents, workspaces);
            if let Some(pane) = weak.upgrade() {
                pane.enter_hub(
                    "Paseo — open an agent, or start one in a project".to_string(),
                    groups,
                );
            }
        })
        .detach();
    }

    fn create_worktree_agent(
        self: &Arc<Self>,
        cwd: String,
        action: &'static str,
        ref_name: Option<String>,
        base_branch: Option<String>,
        provider: String,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.picker = None;
            state.follow = true;
            state.scroll = 0;
            state.status_message = Some(format!("⟳ creating worktree from {cwd}…"));
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let created = client
                .workspace_create_worktree(
                    &cwd,
                    action,
                    ref_name.as_deref(),
                    base_branch.as_deref(),
                    None,
                )
                .await;
            match created {
                Ok(workspace) => {
                    match client
                        .create_agent(&provider, &workspace.cwd, Some(&workspace.id), None)
                        .await
                    {
                        Ok(snapshot) => {
                            if let Some(pane) = weak.upgrade() {
                                pane.load_agent(snapshot);
                            }
                        }
                        Err(err) => {
                            if let Some(pane) = weak.upgrade() {
                                pane.set_status(
                                    "Agent (error)".into(),
                                    Some(format!("create agent: {err}")),
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status("Error".into(), Some(format!("create worktree: {err}")));
                    }
                }
            }
        })
        .detach();
    }

    fn take_pending_delete(&self) -> bool {
        let mut state = self.state.lock();
        match &mut state.picker {
            Some(picker) => std::mem::take(&mut picker.pending_delete),
            None => false,
        }
    }

    fn arm_pending_delete(&self) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                picker.pending_delete = true;
            }
            state.rebuild_rows();
        });
    }

    fn selected_agent_id(&self) -> Option<String> {
        let state = self.state.lock();
        let picker = state.picker.as_ref()?;
        if !matches!(picker.stage, PickerStage::Browse) {
            return None;
        }
        let rows = picker.visible_rows();
        match rows.get(picker.selected)? {
            PickerRow::Entry(gi, ei) => match &picker.groups.get(*gi)?.entries.get(*ei)?.action {
                PickerAction::OpenAgent(id) => Some(id.clone()),
                _ => None,
            },
            PickerRow::Header(_) => None,
        }
    }

    fn picker_delete_key(self: &Arc<Self>) {
        if self.take_pending_delete() {
            let Some(agent_id) = self.selected_agent_id() else {
                self.mutate(|state| state.rebuild_rows());
                return;
            };
            let Some(client) = self.client() else {
                return;
            };
            let weak = self.weak.lock().clone();
            promise::spawn::spawn(async move {
                let _ = client.archive_agent(&agent_id).await;
                if let Some(pane) = weak.upgrade() {
                    pane.picker_refresh();
                }
            })
            .detach();
        } else if self.selected_agent_id().is_some() {
            self.arm_pending_delete();
        }
    }

    fn picker_move(&self, delta: isize) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                if let PickerStage::Input {
                    suggestions,
                    suggestion_selected,
                    ..
                } = &mut picker.stage
                {
                    let len = suggestions.len() as isize;
                    if len > 0 {
                        *suggestion_selected =
                            (*suggestion_selected as isize + delta).rem_euclid(len) as usize;
                    }
                } else {
                    let len = picker.visible_rows().len() as isize;
                    if len > 0 {
                        picker.selected =
                            (picker.selected as isize + delta).rem_euclid(len) as usize;
                    }
                }
            }
            state.rebuild_rows();
        });
    }

    fn picker_page(&self, dir: isize) {
        self.mutate(|state| {
            let page = state.size.rows.saturating_sub(1).max(1) as isize;
            if let Some(picker) = &mut state.picker {
                let len = picker.visible_rows().len() as isize;
                if len == 0 {
                    return;
                }
                let next = (picker.selected as isize + dir * page).clamp(0, len - 1);
                picker.selected = next as usize;
            }
            state.rebuild_rows();
        });
    }

    fn picker_to(&self, end: bool) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                let len = picker.visible_rows().len();
                picker.selected = if end { len.saturating_sub(1) } else { 0 };
            }
            state.rebuild_rows();
        });
    }

    fn picker_fold(&self) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                let group_idx = match picker.visible_rows().get(picker.selected) {
                    Some(PickerRow::Header(gi)) | Some(PickerRow::Entry(gi, _)) => Some(*gi),
                    None => None,
                };
                if let Some(gi) = group_idx {
                    if let Some(group) = picker.groups.get_mut(gi) {
                        group.collapsed = !group.collapsed;
                    }
                    let max = picker.visible_rows().len().saturating_sub(1);
                    picker.selected = picker.selected.min(max);
                }
            }
            state.rebuild_rows();
        });
    }

    fn set_picker_groups(&self, groups: Vec<PickerGroup>) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                picker.groups = groups;
                let max = picker.visible_rows().len().saturating_sub(1);
                picker.selected = picker.selected.min(max);
            }
            state.rebuild_rows();
        });
    }

    fn picker_refresh(self: &Arc<Self>) {
        let kind = self.state.lock().picker.as_ref().map(|p| p.kind);
        match kind {
            Some(PickerKind::Hub) => self.refresh_hub(),
            Some(PickerKind::Connections) => self.set_picker_groups(build_connection_groups()),
            _ => {}
        }
    }

    fn refresh_hub(self: &Arc<Self>) {
        let expanded: Vec<String> = self
            .state
            .lock()
            .picker
            .as_ref()
            .map(|p| {
                p.groups
                    .iter()
                    .filter(|g| !g.collapsed)
                    .map(|g| g.label.clone())
                    .collect()
            })
            .unwrap_or_default();
        let Some(client) = self.client() else {
            return;
        };
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let agents = client.fetch_agents().await.unwrap_or_default();
            let workspaces = client.fetch_workspaces().await.unwrap_or_default();
            let mut groups = build_picker_groups(agents, workspaces);
            for group in &mut groups {
                if expanded.iter().any(|label| label == &group.label) {
                    group.collapsed = false;
                }
            }
            if let Some(pane) = weak.upgrade() {
                pane.set_picker_groups(groups);
            }
        })
        .detach();
    }

    fn picker_input_char(&self, c: char) {
        let mut autocompletes = false;
        self.mutate(|state| {
            if let Some(PickerState {
                stage:
                    PickerStage::Input {
                        kind,
                        buffer,
                        suggestion_selected,
                        ..
                    },
                ..
            }) = &mut state.picker
            {
                buffer.push(c);
                *suggestion_selected = 0;
                autocompletes = kind.autocompletes();
            }
            state.rebuild_rows();
        });
        if autocompletes {
            self.refresh_suggestions();
        }
    }

    fn picker_input_backspace(&self) {
        let mut autocompletes = false;
        self.mutate(|state| {
            if let Some(PickerState {
                stage:
                    PickerStage::Input {
                        kind,
                        buffer,
                        suggestion_selected,
                        ..
                    },
                ..
            }) = &mut state.picker
            {
                buffer.pop();
                *suggestion_selected = 0;
                autocompletes = kind.autocompletes();
            }
            state.rebuild_rows();
        });
        if autocompletes {
            self.refresh_suggestions();
        }
    }

    fn refresh_suggestions(&self) {
        let Some(client) = self.client() else {
            return;
        };
        let (kind, query, context, generation) = {
            let mut state = self.state.lock();
            match &mut state.picker {
                Some(PickerState {
                    stage:
                        PickerStage::Input {
                            kind,
                            buffer,
                            context,
                            suggest_gen,
                            ..
                        },
                    ..
                }) if kind.autocompletes() => {
                    *suggest_gen += 1;
                    (*kind, buffer.clone(), context.clone(), *suggest_gen)
                }
                _ => return,
            }
        };
        let weak = self.weak.lock().clone();
        if kind.is_branch() {
            let Some(cwd) = context else {
                return;
            };
            promise::spawn::spawn(async move {
                let branches = client
                    .branch_suggestions(&cwd, &query, 30)
                    .await
                    .unwrap_or_default();
                if let Some(pane) = weak.upgrade() {
                    pane.apply_suggestions(generation, branches);
                }
            })
            .detach();
            return;
        }
        if !query.starts_with('/') && !query.starts_with('~') {
            self.apply_suggestions(generation, Vec::new());
            return;
        }
        promise::spawn::spawn(async move {
            let directories = client
                .directory_suggestions(&query, 20)
                .await
                .unwrap_or_default();
            if let Some(pane) = weak.upgrade() {
                pane.apply_suggestions(generation, directories);
            }
        })
        .detach();
    }

    fn apply_suggestions(&self, generation: u64, directories: Vec<String>) {
        self.mutate(|state| {
            if let Some(PickerState {
                stage:
                    PickerStage::Input {
                        suggestions,
                        suggestion_selected,
                        suggest_gen,
                        ..
                    },
                ..
            }) = &mut state.picker
            {
                if *suggest_gen == generation {
                    *suggestions = directories;
                    *suggestion_selected = 0;
                }
            }
            state.rebuild_rows();
        });
    }

    fn picker_cancel_input(&self) {
        self.mutate(|state| {
            if let Some(picker) = &mut state.picker {
                picker.stage = PickerStage::Browse;
            }
            state.rebuild_rows();
        });
    }

    fn create_agent_with(self: &Arc<Self>, cwd: String, provider: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.picker = None;
            state.follow = true;
            state.scroll = 0;
            state.status_message = Some(format!(
                "⟳ starting {} in {cwd}…",
                provider_display(&provider)
            ));
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let workspace = client.open_project(&cwd).await.ok();
            match client
                .create_agent(&provider, &cwd, workspace.as_deref(), None)
                .await
            {
                Ok(snapshot) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.load_agent(snapshot);
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status("Agent (error)".into(), Some(format!("create: {err}")));
                    }
                }
            }
        })
        .detach();
    }

    fn open_agent_by_id(self: &Arc<Self>, agent_id: String) {
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
                        pane.set_status("Agent (error)".into(), Some(format!("load: {err}")));
                    }
                }
            }
        })
        .detach();
    }

    fn picker_select(self: &Arc<Self>) {
        enum Chosen {
            Open(String),
            NewIn(String),
            SearchDir,
            InPlace(String),
            MethodBranch(String, WorktreeAction),
            StartInput(InputKind, String, Option<String>),
            RunInput(InputKind, String, Option<String>),
            ToggleGroup(usize),
            ChooseDomain(String),
            SelectProvider(String),
        }
        let chosen = {
            let state = self.state.lock();
            let Some(picker) = &state.picker else {
                return;
            };
            match &picker.stage {
                PickerStage::Input {
                    kind,
                    buffer,
                    context,
                    suggestions,
                    suggestion_selected,
                    ..
                } => {
                    let value = suggestions
                        .get(*suggestion_selected)
                        .cloned()
                        .unwrap_or_else(|| buffer.trim().to_string());
                    Some(Chosen::RunInput(*kind, value, context.clone()))
                }
                PickerStage::Browse => {
                    let rows = picker.visible_rows();
                    match rows.get(picker.selected) {
                        Some(PickerRow::Header(gi)) => Some(Chosen::ToggleGroup(*gi)),
                        Some(PickerRow::Entry(gi, ei)) => picker
                            .groups
                            .get(*gi)
                            .and_then(|g| g.entries.get(*ei))
                            .map(|e| match &e.action {
                                PickerAction::OpenAgent(id) => Chosen::Open(id.clone()),
                                PickerAction::NewAgentInWorkspace(cwd) => {
                                    Chosen::NewIn(cwd.clone())
                                }
                                PickerAction::WorktreeInPlace(cwd) => Chosen::InPlace(cwd.clone()),
                                PickerAction::WorktreeNewBranch(cwd) => {
                                    Chosen::MethodBranch(cwd.clone(), WorktreeAction::BranchOff)
                                }
                                PickerAction::WorktreeCheckoutBranch(cwd) => {
                                    Chosen::MethodBranch(cwd.clone(), WorktreeAction::Checkout)
                                }
                                PickerAction::SearchDirectory => Chosen::SearchDir,
                                PickerAction::NewDirectory => Chosen::StartInput(
                                    InputKind::NewDirectory,
                                    "New directory (full path on the daemon):".to_string(),
                                    None,
                                ),
                                PickerAction::CloneRepo => Chosen::StartInput(
                                    InputKind::CloneRepo,
                                    "Clone GitHub repo (owner/name):".to_string(),
                                    None,
                                ),
                                PickerAction::ChooseDomain(name) => {
                                    Chosen::ChooseDomain(name.clone())
                                }
                                PickerAction::AddConnection => Chosen::StartInput(
                                    InputKind::AddConnection,
                                    "Connect (relay URL or host:port):".to_string(),
                                    None,
                                ),
                                PickerAction::SelectProvider(id) => {
                                    Chosen::SelectProvider(id.clone())
                                }
                            }),
                        None => None,
                    }
                }
            }
        };
        match chosen {
            Some(Chosen::Open(id)) => self.open_agent_by_id(id),
            Some(Chosen::NewIn(cwd)) => {
                let crumb = crumb_basename(&cwd);
                self.wizard_start(WizardStep::Method { cwd }, vec![crumb]);
            }
            Some(Chosen::SearchDir) => self.wizard_start(WizardStep::Directory, Vec::new()),
            Some(Chosen::InPlace(cwd)) => {
                let mut crumbs = self.wizard_top_crumbs();
                crumbs.push("current checkout".to_string());
                self.wizard_to_provider(PendingCreate::Workspace(cwd), crumbs);
            }
            Some(Chosen::MethodBranch(cwd, action)) => {
                let mut crumbs = self.wizard_top_crumbs();
                crumbs.push(
                    match action {
                        WorktreeAction::BranchOff => "new worktree",
                        WorktreeAction::Checkout => "existing branch",
                    }
                    .to_string(),
                );
                self.wizard_push(WizardStep::Branch { cwd, action }, crumbs);
            }
            Some(Chosen::ToggleGroup(gi)) => self.mutate(|state| {
                if let Some(picker) = &mut state.picker {
                    if let Some(group) = picker.groups.get_mut(gi) {
                        group.collapsed = !group.collapsed;
                    }
                    let max = picker.visible_rows().len().saturating_sub(1);
                    picker.selected = picker.selected.min(max);
                }
                state.rebuild_rows();
            }),
            Some(Chosen::StartInput(kind, label, context)) => {
                let buffer = if kind == InputKind::SearchDirectory {
                    "~/".to_string()
                } else {
                    String::new()
                };
                self.mutate(|state| {
                    if let Some(picker) = &mut state.picker {
                        picker.stage = PickerStage::Input {
                            kind,
                            label,
                            buffer,
                            context,
                            suggestions: Vec::new(),
                            suggestion_selected: 0,
                            suggest_gen: 0,
                        };
                    }
                    state.rebuild_rows();
                });
                if kind.autocompletes() {
                    self.refresh_suggestions();
                }
            }
            Some(Chosen::RunInput(kind, value, context)) => {
                if !value.is_empty() {
                    match kind {
                        InputKind::AddConnection => self.run_add_connection(value),
                        InputKind::SearchDirectory => {
                            let crumb = crumb_basename(&value);
                            self.wizard_push(WizardStep::Method { cwd: value }, vec![crumb]);
                        }
                        InputKind::BranchOff => {
                            if let Some(cwd) = context {
                                let mut crumbs = self.wizard_top_crumbs();
                                crumbs.push(value.clone());
                                self.wizard_to_provider(
                                    PendingCreate::WorktreeBranchOff {
                                        cwd,
                                        base_branch: value,
                                    },
                                    crumbs,
                                );
                            }
                        }
                        InputKind::CheckoutBranch => {
                            if let Some(cwd) = context {
                                let mut crumbs = self.wizard_top_crumbs();
                                crumbs.push(value.clone());
                                self.wizard_to_provider(
                                    PendingCreate::WorktreeCheckout {
                                        cwd,
                                        ref_name: value,
                                    },
                                    crumbs,
                                );
                            }
                        }
                        InputKind::NewDirectory => {
                            self.begin_create(PendingCreate::NewDirectory(value))
                        }
                        InputKind::CloneRepo => self.begin_create(PendingCreate::CloneRepo(value)),
                    }
                }
            }
            Some(Chosen::ChooseDomain(name)) => self.open_domain_picker(name),
            Some(Chosen::SelectProvider(provider)) => {
                let pending = self
                    .state
                    .lock()
                    .picker
                    .as_ref()
                    .and_then(|p| p.pending.clone());
                if let Some(pending) = pending {
                    self.wizard_reset();
                    self.execute_create(pending, provider);
                }
            }
            None => {}
        }
    }

    fn open_domain_picker(self: &Arc<Self>, domain: String) {
        let chooser_id = self.pane_id;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let args = KeyAssignment::OpenPaseoAgentPane(PaseoAgentArgs {
                    domain: domain.clone(),
                    chooser: false,
                    agent_id: None,
                    provider: None,
                    cwd: None,
                    prompt: None,
                    new_tab: true,
                    direction: config::keyassignment::PaneDirection::Right,
                    size: config::keyassignment::SplitSize::default(),
                });
                if let Some(pane) = Mux::get()
                    .get_active_tab_for_window(term_window.mux_window_id)
                    .and_then(|tab| tab.get_active_pane())
                {
                    let _ = term_window.perform_key_assignment(&pane, &args);
                }
                Mux::get().remove_pane(chooser_id);
            })));
    }

    fn run_add_connection(self: &Arc<Self>, value: String) {
        let Some((name, target)) = parse_connect_target(&value) else {
            self.set_status(
                "Paseo — connect".into(),
                Some("enter a relay URL (https://…) or host:port".into()),
            );
            return;
        };
        let mux = Mux::get();
        let name = if mux.get_domain_by_name(&name).is_some() {
            name
        } else {
            let domain: Arc<dyn mux::domain::Domain> = paseo_mux::PaseoDomain::new(&name, target);
            mux.add_domain(&domain);
            name
        };
        self.open_domain_picker(name);
    }

    fn run_project_creation(self: &Arc<Self>, kind: InputKind, value: String, provider: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.mutate(|state| {
            state.picker = None;
            state.follow = true;
            state.scroll = 0;
            state.status_message = Some("⟳ creating project…".to_string());
            state.rebuild_rows();
        });
        let weak = self.weak.lock().clone();
        promise::spawn::spawn(async move {
            let cwd = match kind {
                InputKind::NewDirectory => {
                    let (parent, name) = match value.rsplit_once('/') {
                        Some((p, n)) if !p.is_empty() && !n.is_empty() => {
                            (p.to_string(), n.to_string())
                        }
                        _ => {
                            if let Some(pane) = weak.upgrade() {
                                pane.set_status(
                                    "Error".into(),
                                    Some("give a full path like /Users/you/proj".into()),
                                );
                            }
                            return;
                        }
                    };
                    client.project_create_directory(&parent, &name).await
                }
                InputKind::CloneRepo => client.project_github_clone(&value, "https").await,
                InputKind::SearchDirectory
                | InputKind::BranchOff
                | InputKind::CheckoutBranch
                | InputKind::AddConnection => return,
            };
            match cwd {
                Ok(cwd) => {
                    let workspace = client.open_project(&cwd).await.ok();
                    match client
                        .create_agent(&provider, &cwd, workspace.as_deref(), None)
                        .await
                    {
                        Ok(snapshot) => {
                            if let Some(pane) = weak.upgrade() {
                                pane.load_agent(snapshot);
                            }
                        }
                        Err(err) => {
                            if let Some(pane) = weak.upgrade() {
                                pane.set_status(
                                    "Error".into(),
                                    Some(format!("create agent: {err}")),
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    if let Some(pane) = weak.upgrade() {
                        pane.set_status("Error".into(), Some(format!("create project: {err}")));
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
            let num_lines = state.composer.split('\n').count().max(1);
            let (line, col) = state.composer_line_col();
            let first_row = state.size.rows.saturating_sub(num_lines);
            return StableCursorPosition {
                x: 2 + col,
                y: (first_row + line) as StableRowIndex,
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
        let glyph = if is_active_status(&state.agent_status) {
            "● "
        } else if state.pending.is_some() || state.requires_attention {
            "⚠ "
        } else {
            match state.agent_status.as_str() {
                "error" => "✗ ",
                _ => "",
            }
        };
        format!("{glyph}{}", state.title)
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let text = text.to_string();
        self.mutate(|state| {
            if state.picker.is_none() {
                state.mode = Mode::Compose;
                for c in text.chars() {
                    state.composer_insert(if c == '\r' { '\n' } else { c });
                }
                state.rebuild_rows();
            }
        });
        self.scroll_to_bottom();
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
        let picker_input = {
            let state = self.state.lock();
            state
                .picker
                .as_ref()
                .map(|p| matches!(p.stage, PickerStage::Input { .. }))
        };
        if let Some(is_input) = picker_input {
            if is_input {
                match key {
                    KeyCode::Char('\r') | KeyCode::Enter => {
                        if let Some(pane) = self.arc() {
                            pane.picker_select();
                        }
                    }
                    KeyCode::Escape => {
                        if self.wizard_active() {
                            if let Some(pane) = self.arc() {
                                pane.wizard_back();
                            }
                        } else {
                            self.picker_cancel_input();
                        }
                    }
                    KeyCode::Backspace => self.picker_input_backspace(),
                    KeyCode::Char(c) if !c.is_control() && !mods.contains(KeyModifiers::CTRL) => {
                        self.picker_input_char(c)
                    }
                    _ => {}
                }
            } else {
                let ctrl = mods.contains(KeyModifiers::CTRL);
                if !(matches!(key, KeyCode::Char('d')) && !ctrl) {
                    self.take_pending_delete();
                }
                match key {
                    KeyCode::Char('j') | KeyCode::DownArrow => self.picker_move(1),
                    KeyCode::Char('k') | KeyCode::UpArrow => self.picker_move(-1),
                    KeyCode::Char('d') if ctrl => self.picker_page(1),
                    KeyCode::Char('d') => {
                        if let Some(pane) = self.arc() {
                            pane.picker_delete_key();
                        }
                    }
                    KeyCode::Char('u') if ctrl => self.picker_page(-1),
                    KeyCode::PageDown => self.picker_page(1),
                    KeyCode::PageUp => self.picker_page(-1),
                    KeyCode::Char('g') | KeyCode::Home => self.picker_to(false),
                    KeyCode::Char('G') | KeyCode::End => self.picker_to(true),
                    KeyCode::Char('o') | KeyCode::Char('\t') | KeyCode::Tab => self.picker_fold(),
                    KeyCode::Char('r') => {
                        if let Some(pane) = self.arc() {
                            pane.picker_refresh();
                        }
                    }
                    KeyCode::Char('\r') | KeyCode::Enter => {
                        if let Some(pane) = self.arc() {
                            pane.picker_select();
                        }
                    }
                    KeyCode::Escape if self.wizard_active() => {
                        if let Some(pane) = self.arc() {
                            pane.wizard_back();
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Escape => self.close(),
                    _ => {}
                }
            }
            return Ok(());
        }
        let mode = self.state.lock().mode;
        match mode {
            Mode::Compose => match key {
                KeyCode::Char('\r') | KeyCode::Enter
                    if mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT) =>
                {
                    self.mutate(|state| {
                        state.composer_insert('\n');
                        state.rebuild_rows();
                    });
                    self.scroll_to_bottom();
                }
                KeyCode::Char('\r') | KeyCode::Enter => self.submit_composer(),
                KeyCode::Backspace => self.mutate(|state| {
                    state.composer_backspace();
                    state.rebuild_rows();
                }),
                KeyCode::Escape => self.mutate(|state| {
                    state.mode = Mode::Scroll;
                    state.rebuild_rows();
                }),
                KeyCode::LeftArrow => self.mutate(|state| {
                    state.composer_move_horizontal(-1);
                    state.rebuild_rows();
                }),
                KeyCode::RightArrow => self.mutate(|state| {
                    state.composer_move_horizontal(1);
                    state.rebuild_rows();
                }),
                KeyCode::UpArrow => self.mutate(|state| {
                    state.composer_move_vertical(-1);
                    state.rebuild_rows();
                }),
                KeyCode::DownArrow => self.mutate(|state| {
                    state.composer_move_vertical(1);
                    state.rebuild_rows();
                }),
                KeyCode::Home => self.mutate(|state| {
                    state.composer_home();
                    state.rebuild_rows();
                }),
                KeyCode::End => self.mutate(|state| {
                    state.composer_end();
                    state.rebuild_rows();
                }),
                KeyCode::Char(c) if !c.is_control() && !mods.contains(KeyModifiers::CTRL) => self
                    .mutate(|state| {
                        state.composer_insert(c);
                        state.rebuild_rows();
                    }),
                _ => {}
            },
            Mode::Scroll => match key {
                KeyCode::Char('i') | KeyCode::Char('\r') | KeyCode::Enter => {
                    self.mutate(|state| {
                        state.mode = Mode::Compose;
                        state.composer_cursor = state.composer_char_len();
                        state.rebuild_rows();
                    });
                    self.scroll_to_bottom();
                }
                KeyCode::Char('d') if mods.contains(KeyModifiers::CTRL) => self.scroll_page(1),
                KeyCode::Char('u') if mods.contains(KeyModifiers::CTRL) => self.scroll_page(-1),
                KeyCode::Char('d') => self.open_review(),
                KeyCode::Char('t') => self.open_terminal(),
                KeyCode::Char('q') | KeyCode::Escape => self.close(),
                KeyCode::Char(c @ '1'..='9') if self.state.lock().pending.is_some() => {
                    self.respond_action(c as usize - '1' as usize)
                }
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
        let in_picker = {
            let state = self.state.lock();
            matches!(
                &state.picker,
                Some(p) if matches!(p.stage, PickerStage::Browse)
            )
        };
        if event.kind == MouseEventKind::Press {
            match event.button {
                MouseButton::WheelUp(n) if in_picker => self.picker_move(-(n.max(1) as isize)),
                MouseButton::WheelDown(n) if in_picker => self.picker_move(n.max(1) as isize),
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
        let cwd = self.state.lock().cwd.clone();
        if cwd.is_empty() {
            return None;
        }
        Url::from_directory_path(&cwd).ok()
    }
}

pub fn open_paseo_agent_pane(
    term_window: &mut TermWindow,
    args: &PaseoAgentArgs,
) -> anyhow::Result<bool> {
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

    let domain = if args.domain.is_empty() {
        mux.iter_domains()
            .into_iter()
            .find(|d| d.downcast_ref::<paseo_mux::PaseoDomain>().is_some())
            .ok_or_else(|| anyhow!("no paseo domains configured; add one to paseo_daemons"))?
    } else {
        mux.get_domain_by_name(&args.domain)
            .ok_or_else(|| anyhow!("paseo domain {} not found", args.domain))?
    };
    if domain.downcast_ref::<paseo_mux::PaseoDomain>().is_none() {
        anyhow::bail!("domain {} is not a paseo domain", args.domain);
    }

    enum Insertion {
        NewTab,
        Split {
            pane_index: usize,
            request: SplitRequest,
        },
    }

    let (insertion, pane_size) = if args.new_tab {
        (Insertion::NewTab, tab.get_size())
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
                config::keyassignment::SplitSize::Percent(n) => MuxSplitSize::Percent(n),
                config::keyassignment::SplitSize::Cells(n) => MuxSplitSize::Cells(n),
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

    let pane = Arc::new(PaseoAgentPane {
        pane_id: alloc_pane_id(),
        domain_id: domain.domain_id(),
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
            cwd: String::new(),
            workspace_name: None,
            mode: Mode::Scroll,
            composer: String::new(),
            composer_cursor: 0,
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
            size: pane_size,
            seqno: 1,
            dead: false,
            create_stack: Vec::new(),
        }),
    });

    pane.mutate(|state| state.rebuild_rows());
    *pane.weak.lock() = Arc::downgrade(&pane);

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

    if args.chooser {
        pane.start_chooser();
    } else {
        pane.start(AgentSource {
            agent_id: args.agent_id.clone(),
            provider: args.provider.clone(),
            cwd: args.cwd.clone(),
            prompt: args.prompt.clone(),
        });
    }

    Ok(created_tab)
}
