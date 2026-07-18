use crate::termwindow::{TermWindow, TermWindowNotif};
use anyhow::anyhow;
use config::keyassignment::{
    KeyAssignment, PaneDirection, ReviewDiffMode, ReviewModeAssignment, ReviewPaneArgs,
    SpawnCommand, SpawnTabDomain, SplitSize as ConfigSplitSize,
};
use git_review::{
    compute_diff, compute_file_diff, current_branch, find_repo_root, hunk_gap, DiffLimits,
    DiffLineType, DiffMode, GitDiffData, GitFileStatus, Host, Side,
};
use percent_encoding::percent_decode_str;
use std::collections::{HashMap, HashSet};
use termwiz::lineedit::{LineEditBuffer, Movement};
use mux::domain::DomainId;
use mux::pane::{
    alloc_pane_id, impl_for_each_logical_line_via_get_logical_lines,
    impl_get_logical_lines_via_get_lines, CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane,
    PaneId, PerformAssignmentResult, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{SplitDirection, SplitRequest, SplitSize as MuxSplitSize};
use mux::Mux;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use termwiz::cell::{CellAttributes, Intensity};
use termwiz::color::AnsiColor;
use termwiz::surface::{CursorVisibility, Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    unicode_column_width, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    StableRowIndex, TerminalSize,
};
use window::WindowOps;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowKind {
    FileHeader,
    HunkHeader,
    Gap,
    Context,
    Add,
    Delete,
    Summary,
    Info,
    Note,
    NoteEdit,
}

const NOTE_TEXT_COL: usize = 10;
const NOTE_PREFIX: &str = "        ▎ ";
const EDIT_PREFIX: &str = "        ┃ ";

#[derive(Clone, PartialEq, Eq, Hash)]
struct LineAnchor {
    file: String,
    side: Side,
    line: usize,
}

#[derive(Clone)]
struct Comment {
    body: String,
    line_text: String,
}

struct RenderRow {
    text: String,
    kind: RowKind,
    anchor: Option<LineAnchor>,
    payload: Option<String>,
    file: Option<String>,
    commented: bool,
}

impl RenderRow {
    fn plain(text: String, kind: RowKind) -> Self {
        Self {
            text,
            kind,
            anchor: None,
            payload: None,
            file: None,
            commented: false,
        }
    }
}

struct FindState {
    buffer: LineEditBuffer,
    browsing: bool,
}

struct EditState {
    anchor: LineAnchor,
    line_text: String,
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl EditState {
    fn new(anchor: LineAnchor, line_text: String, existing: Option<&String>) -> Self {
        let lines: Vec<String> = match existing {
            Some(text) if !text.is_empty() => text.split('\n').map(|s| s.to_string()).collect(),
            _ => vec![String::new()],
        };
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Self {
            anchor,
            line_text,
            lines,
            row,
            col,
        }
    }

    fn joined(&self) -> String {
        self.lines.join("\n").trim().to_string()
    }

    fn byte_at(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    }

    fn insert_char(&mut self, c: char) {
        let byte = Self::byte_at(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    fn newline(&mut self) {
        let byte = Self::byte_at(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let byte = Self::byte_at(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&removed);
        }
    }

    fn delete(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = Self::byte_at(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_h(&mut self, delta: isize) {
        let len = self.lines[self.row].chars().count();
        if delta < 0 {
            if self.col > 0 {
                self.col -= 1;
            } else if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].chars().count();
            }
        } else if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_v(&mut self, delta: isize) {
        let new_row = self.row as isize + delta;
        if new_row < 0 || new_row as usize >= self.lines.len() {
            return;
        }
        self.row = new_row as usize;
        self.col = self.col.min(self.lines[self.row].chars().count());
    }
}

enum LoadStatus {
    Loading,
    Ready,
    Error(String),
}

enum FileLoad {
    Loading,
    Failed(String),
}

struct ReviewState {
    status: LoadStatus,
    mode: DiffMode,
    host: Host,
    repo_root: String,
    branch: Option<String>,
    data: Option<GitDiffData>,
    cache: HashMap<DiffMode, GitDiffData>,
    file_loads: HashMap<String, FileLoad>,
    compute_seq: u64,
    compute_started: Option<Instant>,
    annotations: HashMap<LineAnchor, Comment>,
    collapsed: HashSet<String>,
    find: Option<FindState>,
    editing: Option<EditState>,
    edit_first_row: Option<usize>,
    pending_d: bool,
    last_refresh: Option<Instant>,
    last_click: Option<(usize, Instant)>,
    select_start: Option<usize>,
    drag_anchor: Option<usize>,
    rows: Vec<RenderRow>,
    rows_version: u64,
    rendered: Vec<Line>,
    rendered_keys: Vec<u64>,
    scroll: usize,
    cursor: usize,
    size: TerminalSize,
    seqno: SequenceNo,
    dead: bool,
    source_pane_id: PaneId,
}

fn find_best_anchor(data: &GitDiffData, anchor: &LineAnchor, line_text: &str) -> Option<LineAnchor> {
    let file = data.files.iter().find(|f| f.file_path == anchor.file)?;
    let mut best: Option<usize> = None;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            let (side, num) = match line.line_type {
                DiffLineType::Add => (Side::New, line.new_line_number),
                DiffLineType::Delete => (Side::Old, line.old_line_number),
                DiffLineType::Context => (Side::New, line.new_line_number),
            };
            if side != anchor.side || line.text != line_text {
                continue;
            }
            if let Some(n) = num {
                let closer = match best {
                    None => true,
                    Some(b) => {
                        (n as isize - anchor.line as isize).abs()
                            < (b as isize - anchor.line as isize).abs()
                    }
                };
                if closer {
                    best = Some(n);
                }
            }
        }
    }
    best.map(|line| LineAnchor {
        file: anchor.file.clone(),
        side: anchor.side,
        line,
    })
}

impl ReviewState {
    fn reanchor_annotations(&mut self) {
        let data = match &self.data {
            Some(d) => d,
            None => return,
        };
        let old = std::mem::take(&mut self.annotations);
        let mut new_map: HashMap<LineAnchor, Comment> = HashMap::new();
        for (anchor, comment) in old {
            let key = find_best_anchor(data, &anchor, &comment.line_text).unwrap_or(anchor);
            new_map.insert(key, comment);
        }
        self.annotations = new_map;
    }

    fn find_bar_line(&self) -> Line {
        let text = match &self.find {
            Some(find) if find.browsing => {
                format!("/{}    n/p: next/prev · Enter/Esc: done", find.buffer.get_line())
            }
            Some(find) => format!("/{}", find.buffer.get_line()),
            None => String::new(),
        };
        let mut a = CellAttributes::default();
        a.set_foreground(AnsiColor::Black);
        a.set_background(AnsiColor::Yellow);
        make_line(&text, &a, 0, self.size.cols)
    }

    fn row_key(&self, doc: usize, is_bar: bool) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.rows_version.hash(&mut h);
        is_bar.hash(&mut h);
        if is_bar {
            if let Some(find) = &self.find {
                find.buffer.get_line().hash(&mut h);
                find.browsing.hash(&mut h);
            }
        } else {
            doc.hash(&mut h);
            (doc == self.cursor).hash(&mut h);
            self.is_selected(doc).hash(&mut h);
        }
        h.finish()
    }

    fn build_view_line(&self, doc: usize, is_bar: bool) -> Line {
        if is_bar {
            return self.find_bar_line();
        }
        let cols = self.size.cols;
        match self.rows.get(doc) {
            Some(row) => {
                let attrs = attrs_for(
                    row.kind,
                    doc == self.cursor,
                    self.is_selected(doc),
                    row.commented,
                );
                make_line(&row.text, &attrs, 0, cols)
            }
            None => make_line("", &CellAttributes::default(), 0, cols),
        }
    }

    fn sync_view(&mut self) {
        let h = self.size.rows;
        if self.rendered.len() != h {
            self.rendered = (0..h)
                .map(|_| make_line("", &CellAttributes::default(), 0, self.size.cols))
                .collect();
            self.rendered_keys = vec![u64::MAX; h];
        }
        let bar_row = h.saturating_sub(1);
        for r in 0..h {
            let is_bar = self.find.is_some() && r == bar_row;
            let doc = self.scroll + r;
            let key = self.row_key(doc, is_bar);
            if self.rendered_keys[r] != key {
                self.rendered[r] = self.build_view_line(doc, is_bar);
                self.rendered_keys[r] = key;
            }
        }
    }

    fn set_collapsed_toggle(&mut self, file: &str) -> Option<(String, GitFileStatus)> {
        let was_collapsed = self.collapsed.remove(file);
        if !was_collapsed {
            self.collapsed.insert(file.to_string());
            return None;
        }
        let candidate = self
            .data
            .as_ref()
            .and_then(|d| d.files.iter().find(|f| f.file_path == file))
            .filter(|f| f.oversized && !f.is_binary)
            .map(|f| (f.file_path.clone(), f.status.clone()))?;
        if self.file_loads.contains_key(&candidate.0) {
            return None;
        }
        self.file_loads.insert(candidate.0.clone(), FileLoad::Loading);
        Some(candidate)
    }

    fn rebuild_rows(&mut self) {
        self.rows_version = self.rows_version.wrapping_add(1);
        if let Some(data) = &self.data {
            self.rows = build_rows(
                data,
                &self.annotations,
                &self.collapsed,
                &self.file_loads,
                self.editing.as_ref(),
            );
        }
        self.edit_first_row = self.rows.iter().position(|r| r.kind == RowKind::NoteEdit);
        match (self.edit_first_row, &self.editing) {
            (Some(first), Some(edit)) => self.cursor = first + edit.row,
            _ => {
                let max = self.rows.len().saturating_sub(1);
                if self.cursor > max {
                    self.cursor = max;
                }
            }
        }
        self.ensure_visible();
    }
}

impl ReviewState {
    fn ensure_visible(&mut self) {
        let h = self.size.rows.max(1);
        let reserved = if self.find.is_some() { 1 } else { 0 };
        let view = h.saturating_sub(reserved).max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + view {
            self.scroll = self.cursor + 1 - view;
        }
        let max_scroll = self.rows.len().saturating_sub(view);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        let c = (self.cursor as isize + delta).clamp(0, max);
        self.cursor = c as usize;
        self.ensure_visible();
    }

    fn page(&mut self, dir: isize) {
        let h = self.size.rows.max(1) as isize;
        self.move_cursor(dir * h);
    }

    fn to_top(&mut self) {
        self.cursor = 0;
        self.ensure_visible();
    }

    fn to_bottom(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
        self.ensure_visible();
    }

    fn jump_to_file(&mut self, query: &str, forward: bool) {
        if query.is_empty() {
            return;
        }
        let q = query.to_lowercase();
        let is_match = |r: &RenderRow| {
            r.kind == RowKind::FileHeader
                && r.file
                    .as_ref()
                    .is_some_and(|f| f.to_lowercase().contains(&q))
        };
        let found = if forward {
            let start = (self.cursor + 1).min(self.rows.len());
            self.rows[start..]
                .iter()
                .position(&is_match)
                .map(|i| start + i)
                .or_else(|| self.rows.iter().position(&is_match))
        } else {
            self.rows[..self.cursor]
                .iter()
                .rposition(&is_match)
                .or_else(|| self.rows.iter().rposition(&is_match))
        };
        if let Some(idx) = found {
            self.cursor = idx;
            self.ensure_visible();
        }
    }

    fn selection_range(&self) -> (usize, usize) {
        match self.select_start {
            Some(start) => (start.min(self.cursor), start.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    fn is_selected(&self, doc: usize) -> bool {
        match self.select_start {
            Some(start) => {
                let (lo, hi) = (start.min(self.cursor), start.max(self.cursor));
                doc >= lo && doc <= hi
            }
            None => false,
        }
    }

    fn jump(&mut self, forward: bool, kinds: &[RowKind]) {
        let matches = |r: &RenderRow| kinds.contains(&r.kind);
        if forward {
            if let Some(i) = (self.cursor + 1..self.rows.len()).find(|&i| matches(&self.rows[i])) {
                self.cursor = i;
            }
        } else if let Some(i) = (0..self.cursor).rev().find(|&i| matches(&self.rows[i])) {
            self.cursor = i;
        }
        self.ensure_visible();
    }
}

pub struct ReviewPane {
    pane_id: PaneId,
    domain_id: DomainId,
    state: Mutex<ReviewState>,
    writer: Mutex<Vec<u8>>,
    window: ::window::Window,
    weak: Mutex<Weak<ReviewPane>>,
}

impl ReviewPane {
    fn mutate<F: FnOnce(&mut ReviewState)>(&self, f: F) {
        {
            let mut state = self.state.lock();
            f(&mut state);
            state.seqno += 1;
        }
        self.window.invalidate();
    }

    fn request_close(&self) {
        {
            let mut state = self.state.lock();
            state.dead = true;
        }
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if let Some(pane) = term_window.get_active_pane_or_overlay() {
                    let _ = term_window.perform_key_assignment(
                        &pane,
                        &KeyAssignment::CloseCurrentPane { confirm: false },
                    );
                }
            })));
    }

    fn apply_action(&self, action: &ReviewModeAssignment) {
        use ReviewModeAssignment::*;
        match action {
            MoveUp => self.mutate(|s| s.move_cursor(-1)),
            MoveDown => self.mutate(|s| s.move_cursor(1)),
            PageUp => self.mutate(|s| s.page(-1)),
            PageDown => self.mutate(|s| s.page(1)),
            MoveToTop => self.mutate(|s| s.to_top()),
            MoveToBottom => self.mutate(|s| s.to_bottom()),
            NextHunk => self.mutate(|s| s.jump(true, &[RowKind::HunkHeader, RowKind::FileHeader])),
            PrevHunk => self.mutate(|s| s.jump(false, &[RowKind::HunkHeader, RowKind::FileHeader])),
            NextFile => self.mutate(|s| s.jump(true, &[RowKind::FileHeader])),
            PrevFile => self.mutate(|s| s.jump(false, &[RowKind::FileHeader])),
            Annotate => self.start_edit(true),
            ToggleSelect => self.toggle_select(),
            SendSelection => self.send_selection(),
            SendAll => self.send_comments(),
            ToggleFold => self.toggle_fold(),
            FindFile => self.start_find(),
            CycleDiffMode => self.cycle_diff_mode(),
            Refresh => self.recompute(false),
            Close => self.request_close(),
        }
    }

    fn recompute(&self, reset_view: bool) {
        let arc = match self.weak.lock().upgrade() {
            Some(arc) => arc,
            None => return,
        };
        {
            let mut s = self.state.lock();
            s.select_start = None;
            s.find = None;
            s.editing = None;
            s.edit_first_row = None;
            s.last_refresh = Some(Instant::now());
            s.compute_seq = s.compute_seq.wrapping_add(1);
        }
        ReviewPane::spawn_compute(arc, reset_view);
    }

    fn cycle_diff_mode(&self) {
        {
            let mut s = self.state.lock();
            s.mode = s.mode.cycle();
            if let Some(data) = s.cache.get(&s.mode).cloned() {
                s.collapsed = data.files.iter().map(|f| f.file_path.clone()).collect();
                s.file_loads.clear();
                s.data = Some(data);
                s.status = LoadStatus::Ready;
                s.cursor = 0;
                s.scroll = 0;
                s.rebuild_rows();
            }
        }
        self.recompute(true);
    }

    fn toggle_select(&self) {
        self.mutate(|s| {
            if s.select_start.is_some() {
                s.select_start = None;
            } else {
                s.select_start = Some(s.cursor);
            }
        });
    }

    fn send_payload(&self, payload: String) {
        if payload.trim().is_empty() {
            return;
        }
        let target = self.state.lock().source_pane_id;
        if let Some(pane) = Mux::get().get_pane(target) {
            let _ = pane.send_paste(&payload);
        }
        self.mutate(|s| s.select_start = None);
    }

    fn send_selection(&self) {
        let payload = {
            let s = self.state.lock();
            let (lo, hi) = s.selection_range();
            build_send_payload(&s.rows, &s.annotations, lo, hi)
        };
        self.send_payload(payload);
    }

    fn send_comments(&self) {
        let payload = build_comment_payload(&self.state.lock().annotations);
        self.send_payload(payload);
    }

    fn current_anchor(&self, s: &ReviewState) -> Option<LineAnchor> {
        s.rows.get(s.cursor).and_then(|r| r.anchor.clone())
    }

    fn start_edit(&self, at_end: bool) {
        self.mutate(|s| {
            if s.editing.is_some() {
                return;
            }
            if let Some(anchor) = s.rows.get(s.cursor).and_then(|r| r.anchor.clone()) {
                let existing = s.annotations.get(&anchor);
                let line_text = existing
                    .map(|c| c.line_text.clone())
                    .or_else(|| {
                        s.rows[s.cursor]
                            .payload
                            .as_ref()
                            .map(|p| p.chars().skip(1).collect::<String>())
                    })
                    .unwrap_or_default();
                let body = existing.map(|c| c.body.clone());
                let mut edit = EditState::new(anchor, line_text, body.as_ref());
                if !at_end {
                    edit.row = 0;
                    edit.col = 0;
                }
                s.editing = Some(edit);
                s.select_start = None;
                s.rebuild_rows();
            }
        });
    }

    fn edit_if_comment(&self) -> bool {
        let has = {
            let s = self.state.lock();
            self.current_anchor(&s)
                .is_some_and(|a| s.annotations.contains_key(&a))
        };
        if has {
            self.start_edit(true);
        }
        has
    }

    fn open_editor(&self) {
        let (repo_root, path_str, line) = {
            let s = self.state.lock();
            if !s.host.is_local() {
                return;
            }
            let row = match s.rows.get(s.cursor) {
                Some(r) => r,
                None => return,
            };
            if !matches!(row.kind, RowKind::Add | RowKind::Delete | RowKind::Context) {
                return;
            }
            let anchor = match &row.anchor {
                Some(a) => a.clone(),
                None => return,
            };
            let path = std::path::Path::new(&s.repo_root).join(&anchor.file);
            (
                PathBuf::from(&s.repo_root),
                path.to_string_lossy().to_string(),
                anchor.line,
            )
        };

        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "nvim".to_string());

        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let pane = match term_window.get_active_pane_or_overlay() {
                    Some(p) => p,
                    None => return,
                };
                let command = SpawnCommand {
                    label: None,
                    args: Some(vec![editor, format!("+{line}"), path_str]),
                    cwd: Some(repo_root),
                    set_environment_variables: HashMap::new(),
                    domain: SpawnTabDomain::CurrentPaneDomain,
                    position: None,
                };
                let action = KeyAssignment::SplitPane(config::keyassignment::SplitPane {
                    direction: PaneDirection::Down,
                    size: ConfigSplitSize::default(),
                    command,
                    top_level: false,
                });
                let _ = term_window.perform_key_assignment(&pane, &action);
            })));
    }

    fn delete_comment(&self) {
        self.mutate(|s| {
            if let Some(anchor) = s.rows.get(s.cursor).and_then(|r| r.anchor.clone()) {
                if s.annotations.remove(&anchor).is_some() {
                    s.rebuild_rows();
                }
            }
        });
    }

    fn cancel_edit(&self) {
        self.mutate(|s| {
            s.editing = None;
            s.edit_first_row = None;
            s.rebuild_rows();
        });
    }

    fn commit_edit(&self) {
        self.mutate(|s| {
            if let Some(edit) = s.editing.take() {
                let text = edit.joined();
                if text.is_empty() {
                    s.annotations.remove(&edit.anchor);
                } else {
                    s.annotations.insert(
                        edit.anchor,
                        Comment {
                            body: text,
                            line_text: edit.line_text,
                        },
                    );
                }
                s.edit_first_row = None;
                s.rebuild_rows();
            }
        });
    }

    fn edit_apply<F: FnOnce(&mut EditState)>(&self, f: F) {
        self.mutate(|s| {
            if let Some(edit) = &mut s.editing {
                f(edit);
            }
            s.rebuild_rows();
        });
    }

    fn handle_edit_key(&self, key: KeyCode, mods: KeyModifiers) {
        let confirm = mods.contains(KeyModifiers::SHIFT);
        match key {
            KeyCode::Char('\x1b') | KeyCode::Escape => self.cancel_edit(),
            KeyCode::Char('\r') | KeyCode::Enter if confirm => self.commit_edit(),
            KeyCode::Char('\r') | KeyCode::Enter => self.edit_apply(|e| e.newline()),
            KeyCode::Backspace => self.edit_apply(|e| e.backspace()),
            KeyCode::Delete => self.edit_apply(|e| e.delete()),
            KeyCode::LeftArrow => self.edit_apply(|e| e.move_h(-1)),
            KeyCode::RightArrow => self.edit_apply(|e| e.move_h(1)),
            KeyCode::UpArrow => self.edit_apply(|e| e.move_v(-1)),
            KeyCode::DownArrow => self.edit_apply(|e| e.move_v(1)),
            KeyCode::Home => self.edit_apply(|e| e.col = 0),
            KeyCode::End => self.edit_apply(|e| e.col = e.lines[e.row].chars().count()),
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CTRL) => {
                self.edit_apply(|e| e.insert_char(c))
            }
            _ => {}
        }
    }

    fn start_find(&self) {
        self.mutate(|s| {
            if s.find.is_some() {
                return;
            }
            s.find = Some(FindState {
                buffer: LineEditBuffer::new("", 0),
                browsing: false,
            });
        });
    }

    fn find_jump(&self, forward: bool) {
        self.mutate(|s| {
            if let Some(find) = &s.find {
                let text = find.buffer.get_line().trim().to_string();
                s.jump_to_file(&text, forward);
                if let Some(find) = &mut s.find {
                    find.browsing = true;
                }
            }
        });
    }

    fn cancel_find(&self) {
        self.mutate(|s| {
            s.find = None;
        });
    }

    fn find_apply<F: FnOnce(&mut LineEditBuffer)>(&self, f: F) {
        self.mutate(|s| {
            if let Some(find) = &mut s.find {
                f(&mut find.buffer);
            }
        });
    }

    fn handle_find_key(&self, key: KeyCode, mods: KeyModifiers) {
        let browsing = self
            .state
            .lock()
            .find
            .as_ref()
            .is_some_and(|f| f.browsing);
        if browsing {
            match key {
                KeyCode::Char('n') => self.find_jump(true),
                KeyCode::Char('p') | KeyCode::Char('N') => self.find_jump(false),
                KeyCode::Char('\r') | KeyCode::Enter | KeyCode::Char('\x1b') | KeyCode::Escape => {
                    self.cancel_find()
                }
                _ => self.mutate(|s| {
                    if let Some(f) = &mut s.find {
                        f.browsing = false;
                        if let KeyCode::Char(c) = key {
                            if !mods.contains(KeyModifiers::CTRL) {
                                f.buffer.insert_char(c);
                            }
                        }
                    }
                }),
            }
            return;
        }
        match key {
            KeyCode::Char('\r') | KeyCode::Enter => self.find_jump(true),
            KeyCode::Char('\x1b') | KeyCode::Escape => self.cancel_find(),
            KeyCode::Backspace => {
                self.find_apply(|b| b.kill_text(Movement::BackwardChar(1), Movement::BackwardChar(1)))
            }
            KeyCode::LeftArrow => self.find_apply(|b| b.exec_movement(Movement::BackwardChar(1))),
            KeyCode::RightArrow => self.find_apply(|b| b.exec_movement(Movement::ForwardChar(1))),
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CTRL) => {
                self.find_apply(|b| b.insert_char(c))
            }
            _ => {}
        }
    }

    fn toggle_fold(&self) {
        let mut to_load = None;
        self.mutate(|s| {
            if let Some(file) = s.rows.get(s.cursor).and_then(|r| r.file.clone()) {
                to_load = s.set_collapsed_toggle(&file);
                s.rebuild_rows();
                if let Some(idx) = s.rows.iter().position(|r| {
                    r.kind == RowKind::FileHeader && r.file.as_deref() == Some(file.as_str())
                }) {
                    s.cursor = idx;
                    s.ensure_visible();
                }
            }
        });
        if let Some((path, status)) = to_load {
            if let Some(arc) = self.weak.lock().upgrade() {
                ReviewPane::spawn_file_load(arc, path, status);
            }
        }
    }

    fn render_find_bar(&self, state: &ReviewState) -> Line {
        let text = match &state.find {
            Some(find) if find.browsing => {
                format!("/{}    n/p: next/prev · Enter/Esc: done", find.buffer.get_line())
            }
            Some(find) => format!("/{}", find.buffer.get_line()),
            None => String::new(),
        };
        let mut a = CellAttributes::default();
        a.set_foreground(AnsiColor::Black);
        a.set_background(AnsiColor::Yellow);
        make_line(&text, &a, state.seqno, state.size.cols)
    }

    fn render_doc_row(&self, state: &ReviewState, doc: usize) -> Line {
        let cols = state.size.cols;
        if doc >= state.rows.len() {
            return make_line("", &CellAttributes::default(), state.seqno, cols);
        }
        let row = &state.rows[doc];
        let attrs = attrs_for(row.kind, doc == state.cursor, state.is_selected(doc), row.commented);
        make_line(&row.text, &attrs, state.seqno, cols)
    }

    fn spawn_compute(pane: Arc<ReviewPane>, reset_view: bool) {
        let (host, start, mode, seq, is_initial) = {
            let mut s = pane.state.lock();
            s.compute_started = Some(Instant::now());
            let is_initial = s.data.is_none();
            if is_initial {
                s.status = LoadStatus::Loading;
            }
            (
                s.host.clone(),
                s.repo_root.clone(),
                s.mode.clone(),
                s.compute_seq,
                is_initial,
            )
        };

        if is_initial {
            Self::spawn_loading_ticker(pane.clone(), seq);
        }

        std::thread::spawn(move || {
            let result = (|| {
                let root = find_repo_root(&host, &start)?;
                let branch = current_branch(&host, &root);
                let data = compute_diff(&host, &root, &mode, &DiffLimits::default())?;
                Ok::<_, anyhow::Error>((root, branch, data))
            })();

            {
                let mut state = pane.state.lock();
                if state.compute_seq != seq {
                    return;
                }
                match result {
                    Ok((root, branch, data)) => {
                        state.repo_root = root;
                        state.branch = branch;
                        state.cache.insert(mode.clone(), data.clone());
                        if reset_view {
                            state.collapsed =
                                data.files.iter().map(|f| f.file_path.clone()).collect();
                            state.cursor = 0;
                            state.scroll = 0;
                            state.file_loads.clear();
                        }
                        state.data = Some(data);
                        state.status = LoadStatus::Ready;
                        if !reset_view {
                            state.reanchor_annotations();
                        }
                        state.rebuild_rows();
                    }
                    Err(err) => {
                        let msg = match &host {
                            Host::Ssh(remote) => format!(
                                "Review requires a local git repository; this pane's working directory is on {remote}."
                            ),
                            Host::Local => format!("error: {err:#}"),
                        };
                        state.rows = vec![RenderRow::plain(msg.clone(), RowKind::Info)];
                        state.status = LoadStatus::Error(msg);
                        state.cursor = 0;
                        state.scroll = 0;
                    }
                }
                state.seqno += 1;
            }
            pane.window.invalidate();
        });
    }

    fn spawn_loading_ticker(pane: Arc<ReviewPane>, seq: u64) {
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            {
                let mut s = pane.state.lock();
                if s.compute_seq != seq || !matches!(s.status, LoadStatus::Loading) {
                    break;
                }
                let elapsed = s.compute_started.map_or(0, |t| t.elapsed().as_secs());
                let hint = if elapsed >= 5 {
                    " — scanning a large tree can take a while"
                } else {
                    ""
                };
                s.rows = vec![RenderRow::plain(
                    format!("⟳ computing diff… ({elapsed}s){hint}"),
                    RowKind::Info,
                )];
                s.seqno += 1;
            }
            pane.window.invalidate();
        });
    }

    fn spawn_file_load(pane: Arc<ReviewPane>, path: String, status: GitFileStatus) {
        std::thread::spawn(move || {
            let (host, repo, mode, seq) = {
                let s = pane.state.lock();
                (s.host.clone(), s.repo_root.clone(), s.mode.clone(), s.compute_seq)
            };
            let result = compute_file_diff(&host, &repo, &mode, &path, &status, &DiffLimits::on_demand());

            {
                let mut s = pane.state.lock();
                if s.compute_seq != seq {
                    return;
                }
                match result {
                    Ok(file) => {
                        let still_oversized = file.oversized;
                        if let Some(data) = &mut s.data {
                            if let Some(slot) =
                                data.files.iter_mut().find(|f| f.file_path == path)
                            {
                                *slot = file;
                            }
                            data.recompute_totals();
                        }
                        if still_oversized {
                            s.file_loads
                                .insert(path.clone(), FileLoad::Failed("file too large to display".to_string()));
                        } else {
                            s.file_loads.remove(&path);
                        }
                    }
                    Err(err) => {
                        s.file_loads.insert(path.clone(), FileLoad::Failed(format!("{err:#}")));
                    }
                }
                s.seqno += 1;
                s.rebuild_rows();
            }
            pane.window.invalidate();
        });
    }
}

fn make_line(text: &str, attrs: &CellAttributes, seqno: SequenceNo, cols: usize) -> Line {
    let width = unicode_column_width(text, None);
    let padded = if width < cols {
        format!("{text}{}", " ".repeat(cols - width))
    } else {
        text.to_string()
    };
    Line::from_text(&padded, attrs, seqno, None)
}

fn attrs_for(kind: RowKind, cursor: bool, selected: bool, commented: bool) -> CellAttributes {
    let mut a = CellAttributes::default();
    match kind {
        RowKind::Add => {
            a.set_foreground(AnsiColor::Green);
        }
        RowKind::Delete => {
            a.set_foreground(AnsiColor::Maroon);
        }
        RowKind::HunkHeader => {
            a.set_foreground(AnsiColor::Teal);
        }
        RowKind::FileHeader => {
            a.set_foreground(if commented {
                AnsiColor::Yellow
            } else {
                AnsiColor::White
            });
            a.set_intensity(Intensity::Bold);
        }
        RowKind::Gap => {
            a.set_foreground(AnsiColor::Silver);
        }
        RowKind::Summary => {
            a.set_foreground(AnsiColor::Olive);
        }
        RowKind::Info => {
            a.set_foreground(AnsiColor::Silver);
        }
        RowKind::Note => {
            a.set_foreground(AnsiColor::Yellow);
        }
        RowKind::NoteEdit => {
            a.set_foreground(AnsiColor::White);
            a.set_background(AnsiColor::Navy);
        }
        RowKind::Context => {}
    }
    if cursor {
        a.set_reverse(true);
    } else if selected {
        a.set_background(AnsiColor::Navy);
    }
    a
}

fn status_glyph(status: &GitFileStatus) -> char {
    match status {
        GitFileStatus::New | GitFileStatus::Untracked => 'A',
        GitFileStatus::Modified => 'M',
        GitFileStatus::Deleted => 'D',
        GitFileStatus::Renamed { .. } => 'R',
        GitFileStatus::Copied { .. } => 'C',
        GitFileStatus::Conflicted => 'U',
    }
}

fn line_anchor(file: &str, line: &git_review::DiffLine) -> Option<LineAnchor> {
    match line.line_type {
        DiffLineType::Add => line.new_line_number.map(|n| LineAnchor {
            file: file.to_string(),
            side: Side::New,
            line: n,
        }),
        DiffLineType::Delete => line.old_line_number.map(|n| LineAnchor {
            file: file.to_string(),
            side: Side::Old,
            line: n,
        }),
        DiffLineType::Context => line.new_line_number.map(|n| LineAnchor {
            file: file.to_string(),
            side: Side::New,
            line: n,
        }),
    }
}

fn build_send_payload(
    rows: &[RenderRow],
    annotations: &HashMap<LineAnchor, Comment>,
    lo: usize,
    hi: usize,
) -> String {
    let mut out = String::new();
    let mut last_file: Option<String> = None;
    for row in rows.iter().take(hi + 1).skip(lo) {
        let payload = match &row.payload {
            Some(p) => p,
            None => continue,
        };
        if let Some(anchor) = &row.anchor {
            if last_file.as_deref() != Some(anchor.file.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&anchor.file);
                out.push('\n');
                last_file = Some(anchor.file.clone());
            }
        }
        out.push_str(payload);
        out.push('\n');
        if let Some(anchor) = &row.anchor {
            if let Some(note) = annotations.get(anchor) {
                for (i, line) in note.body.split('\n').enumerate() {
                    if i == 0 {
                        out.push_str(&format!("# note: {line}\n"));
                    } else {
                        out.push_str(&format!("#       {line}\n"));
                    }
                }
            }
        }
    }
    out
}

fn build_comment_payload(annotations: &HashMap<LineAnchor, Comment>) -> String {
    let mut items: Vec<(&str, usize, &str)> = annotations
        .iter()
        .map(|(a, c)| (a.file.as_str(), a.line, c.body.as_str()))
        .collect();
    items.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));

    let mut out = String::new();
    let mut last_file: Option<&str> = None;
    for (file, line, note) in items {
        if last_file != Some(file) {
            if !out.is_empty() {
                out.push('\n');
            }
            last_file = Some(file);
        }
        let mut note_lines = note.split('\n');
        let first = note_lines.next().unwrap_or("");
        out.push_str(&format!("{file}:{line}: {first}\n"));
        for line in note_lines {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out
}

fn note_row(text: String, kind: RowKind, anchor: &LineAnchor) -> RenderRow {
    RenderRow {
        text,
        kind,
        anchor: Some(anchor.clone()),
        payload: None,
        file: None,
        commented: false,
    }
}

fn push_note_rows(rows: &mut Vec<RenderRow>, anchor: &LineAnchor, editing: Option<&EditState>, saved: Option<&Comment>) {
    match editing {
        Some(edit) if &edit.anchor == anchor => {
            for line in &edit.lines {
                rows.push(note_row(format!("{EDIT_PREFIX}{line}"), RowKind::NoteEdit, anchor));
            }
        }
        _ => {
            if let Some(note) = saved {
                for line in note.body.split('\n') {
                    rows.push(note_row(format!("{NOTE_PREFIX}{line}"), RowKind::Note, anchor));
                }
            }
        }
    }
}

fn push_orphan_rows(
    rows: &mut Vec<RenderRow>,
    file: &str,
    annotations: &HashMap<LineAnchor, Comment>,
    rendered: &HashSet<LineAnchor>,
) {
    let mut orphans: Vec<(&LineAnchor, &Comment)> = annotations
        .iter()
        .filter(|(a, _)| a.file == file && !rendered.contains(a))
        .collect();
    orphans.sort_by_key(|(a, _)| a.line);
    for (anchor, comment) in orphans {
        for (i, line) in comment.body.split('\n').enumerate() {
            let text = if i == 0 {
                format!("        ⚠ (outdated) {line}")
            } else {
                format!("{NOTE_PREFIX}{line}")
            };
            rows.push(note_row(text, RowKind::Summary, anchor));
        }
    }
}

fn build_rows(
    data: &GitDiffData,
    annotations: &HashMap<LineAnchor, Comment>,
    collapsed: &HashSet<String>,
    file_loads: &HashMap<String, FileLoad>,
    editing: Option<&EditState>,
) -> Vec<RenderRow> {
    let mut rows = Vec::new();
    if data.files.is_empty() {
        rows.push(RenderRow::plain(
            "No changes in working tree".to_string(),
            RowKind::Info,
        ));
        return rows;
    }

    for file in &data.files {
        let folded = collapsed.contains(&file.file_path);
        let indicator = if folded { '▸' } else { '▾' };
        let comment_count = annotations
            .keys()
            .filter(|a| a.file == file.file_path)
            .count();
        let comment_tag = if comment_count > 0 {
            format!("  💬 {comment_count}")
        } else {
            String::new()
        };
        rows.push(RenderRow {
            text: format!(
                "{} {} {}  +{} -{}{}",
                indicator,
                status_glyph(&file.status),
                file.file_path,
                file.additions,
                file.deletions,
                comment_tag
            ),
            kind: RowKind::FileHeader,
            anchor: None,
            payload: None,
            file: Some(file.file_path.clone()),
            commented: comment_count > 0,
        });

        if folded {
            continue;
        }

        let mut rendered_anchors: HashSet<LineAnchor> = HashSet::new();
        if file.is_binary {
            rows.push(RenderRow::plain("  binary file".to_string(), RowKind::Summary));
        } else if file.oversized {
            let msg = match file_loads.get(&file.file_path) {
                Some(FileLoad::Loading) => "  loading…".to_string(),
                Some(FileLoad::Failed(err)) => format!("  {err}"),
                None => match file.status {
                    GitFileStatus::Untracked => {
                        "  large untracked file — press o/Tab to load".to_string()
                    }
                    _ => format!(
                        "  large diff (+{} -{}) — press o/Tab to load",
                        file.additions, file.deletions
                    ),
                },
            };
            rows.push(RenderRow::plain(msg, RowKind::Summary));
        } else {
            for (hi, hunk) in file.hunks.iter().enumerate() {
                if hi > 0 {
                    let gap = hunk_gap(&file.hunks[hi - 1], hunk);
                    rows.push(RenderRow::plain(
                        format!("  ⋯ {gap} unchanged lines"),
                        RowKind::Gap,
                    ));
                }
                rows.push(RenderRow::plain(hunk.header_text(), RowKind::HunkHeader));
                for line in &hunk.lines {
                    let old = line
                        .old_line_number
                        .map(|n| format!("{n:>5}"))
                        .unwrap_or_else(|| "     ".to_string());
                    let new = line
                        .new_line_number
                        .map(|n| format!("{n:>5}"))
                        .unwrap_or_else(|| "     ".to_string());
                    let kind = match line.line_type {
                        DiffLineType::Add => RowKind::Add,
                        DiffLineType::Delete => RowKind::Delete,
                        DiffLineType::Context => RowKind::Context,
                    };
                    let anchor = line_anchor(&file.file_path, line);
                    let marked = anchor.as_ref().is_some_and(|a| annotations.contains_key(a));
                    let glyph = if marked { '●' } else { ' ' };
                    rows.push(RenderRow {
                        text: format!("{glyph}{old} {new} {}{}", line.marker(), line.text),
                        kind,
                        anchor: anchor.clone(),
                        payload: Some(format!("{}{}", line.marker(), line.text)),
                        file: Some(file.file_path.clone()),
                        commented: false,
                    });
                    if let Some(a) = &anchor {
                        rendered_anchors.insert(a.clone());
                        push_note_rows(&mut rows, a, editing, annotations.get(a));
                    }
                }
            }
        }

        push_orphan_rows(&mut rows, &file.file_path, annotations, &rendered_anchors);
        rows.push(RenderRow::plain(String::new(), RowKind::Context));
    }

    rows
}

fn config_mode_to_diff(mode: &ReviewDiffMode) -> DiffMode {
    match mode {
        ReviewDiffMode::WorkingTree => DiffMode::WorkingTree,
        ReviewDiffMode::Staged => DiffMode::Staged,
        ReviewDiffMode::Branch(b) => DiffMode::Branch(b.clone()),
        ReviewDiffMode::MergeBase(b) => DiffMode::MergeBase(b.clone()),
    }
}

pub fn open_review_pane(
    term_window: &mut TermWindow,
    args: &ReviewPaneArgs,
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

    let (split_direction, target_is_second) = match args.direction {
        PaneDirection::Right => (SplitDirection::Horizontal, true),
        PaneDirection::Left => (SplitDirection::Horizontal, false),
        PaneDirection::Down => (SplitDirection::Vertical, true),
        PaneDirection::Up => (SplitDirection::Vertical, false),
        PaneDirection::Next | PaneDirection::Prev => {
            anyhow::bail!("invalid direction for review pane");
        }
    };

    let request = SplitRequest {
        direction: split_direction,
        target_is_second,
        size: match args.size {
            ConfigSplitSize::Percent(n) => MuxSplitSize::Percent(n),
            ConfigSplitSize::Cells(n) => MuxSplitSize::Cells(n),
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

    let (host, start_path) = resolve_source_location(&source);

    let review = Arc::new(ReviewPane {
        pane_id: alloc_pane_id(),
        domain_id: source.domain_id(),
        writer: Mutex::new(Vec::new()),
        window: window.clone(),
        weak: Mutex::new(Weak::new()),
        state: Mutex::new(ReviewState {
            status: LoadStatus::Loading,
            mode: config_mode_to_diff(&args.mode),
            host,
            repo_root: start_path,
            branch: None,
            data: None,
            cache: HashMap::new(),
            file_loads: HashMap::new(),
            compute_seq: 0,
            compute_started: None,
            annotations: HashMap::new(),
            collapsed: HashSet::new(),
            find: None,
            editing: None,
            edit_first_row: None,
            pending_d: false,
            last_refresh: None,
            last_click: None,
            select_start: None,
            drag_anchor: None,
            rows_version: 0,
            rendered: Vec::new(),
            rendered_keys: Vec::new(),
            rows: vec![RenderRow::plain(
                "⟳ computing diff…".to_string(),
                RowKind::Info,
            )],
            scroll: 0,
            cursor: 0,
            size: split_size.second,
            seqno: 1,
            dead: false,
            source_pane_id,
        }),
    });

    *review.weak.lock() = Arc::downgrade(&review);

    let pane: Arc<dyn Pane> = review.clone();
    mux.add_pane(&pane)?;
    tab.split_and_insert(pane_index, request, pane)?;

    ReviewPane::spawn_compute(review, true);

    Ok(())
}

fn is_local_host(host: &str) -> bool {
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match hostname::get().ok().and_then(|h| h.into_string().ok()) {
        Some(local) => {
            let remote_label = host.split('.').next().unwrap_or(host);
            let local_label = local.split('.').next().unwrap_or(&local);
            remote_label.eq_ignore_ascii_case(local_label)
        }
        None => false,
    }
}

fn resolve_source_location(source: &Arc<dyn Pane>) -> (Host, String) {
    if let Some(url) = source.get_current_working_dir(CachePolicy::FetchImmediate) {
        if url.scheme() == "file" {
            let host = url.host_str().unwrap_or("");
            let path = percent_decode_str(url.path())
                .decode_utf8_lossy()
                .into_owned();
            if is_local_host(host) {
                return (Host::Local, path);
            }
            return (Host::Ssh(host.to_string()), path);
        }
    }
    let fallback = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    (Host::Local, fallback)
}

impl Pane for ReviewPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let state = self.state.lock();
        if let (Some(edit), Some(first)) = (&state.editing, state.edit_first_row) {
            let y = (first + edit.row).saturating_sub(state.scroll) as StableRowIndex;
            return StableCursorPosition {
                x: NOTE_TEXT_COL + edit.col,
                y,
                shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: CursorVisibility::Visible,
            };
        }
        if let Some(find) = &state.find {
            if !find.browsing {
                let bar_row = state.size.rows.saturating_sub(1);
                return StableCursorPosition {
                    x: 1 + find.buffer.get_cursor(),
                    y: bar_row as StableRowIndex,
                    shape: termwiz::surface::CursorShape::SteadyBlock,
                    visibility: CursorVisibility::Visible,
                };
            }
        }
        let y = state.cursor.saturating_sub(state.scroll) as StableRowIndex;
        StableCursorPosition {
            x: 0,
            y,
            shape: termwiz::surface::CursorShape::Default,
            visibility: CursorVisibility::Hidden,
        }
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

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let state = self.state.lock();
        let start = lines.start.max(0);
        let bar_row = state.size.rows.saturating_sub(1);
        let mut out = Vec::new();
        for r in start..lines.end.max(start) {
            if state.find.is_some() && r as usize == bar_row {
                out.push(self.render_find_bar(&state));
            } else {
                let doc = state.scroll + r as usize;
                out.push(self.render_doc_row(&state, doc));
            }
        }
        (start, out)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let mut state = self.state.lock();
        state.sync_view();
        let h = state.rendered.len() as StableRowIndex;
        let start = lines.start.clamp(0, h);
        let end = lines.end.clamp(start, h);
        let mut refs: Vec<&mut Line> = state.rendered[start as usize..end as usize]
            .iter_mut()
            .collect();
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

    fn apply_hyperlinks(&self, _lines: Range<StableRowIndex>, _rules: &[termwiz::hyperlink::Rule]) {}

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
        match &state.status {
            LoadStatus::Loading => "Review (computing…)".to_string(),
            LoadStatus::Error(msg) => format!("Review error: {msg}"),
            LoadStatus::Ready => {
                let branch = state.branch.as_deref().unwrap_or("");
                format!("Review {} [{}]", branch, state.mode.label())
            }
        }
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.mutate(|state| {
            state.size = size;
            state.ensure_visible();
        });
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let (editing, finding) = {
            let s = self.state.lock();
            (s.editing.is_some(), s.find.is_some())
        };
        if editing {
            self.handle_edit_key(key, mods);
            return Ok(());
        }
        if finding {
            self.handle_find_key(key, mods);
            return Ok(());
        }
        let ctrl = mods.contains(KeyModifiers::CTRL);
        if !(matches!(key, KeyCode::Char('d')) && !ctrl) {
            self.state.lock().pending_d = false;
        }
        match key {
            KeyCode::Char('j') | KeyCode::DownArrow => self.mutate(|s| s.move_cursor(1)),
            KeyCode::Char('k') | KeyCode::UpArrow => self.mutate(|s| s.move_cursor(-1)),
            KeyCode::Char('n') => self.apply_action(&ReviewModeAssignment::NextHunk),
            KeyCode::Char('p') => self.apply_action(&ReviewModeAssignment::PrevHunk),
            KeyCode::Char('N') => self.apply_action(&ReviewModeAssignment::NextFile),
            KeyCode::Char('P') => self.apply_action(&ReviewModeAssignment::PrevFile),
            KeyCode::Char('i') => self.start_edit(false),
            KeyCode::Char('a') => self.start_edit(true),
            KeyCode::Char('e') => self.open_editor(),
            KeyCode::Char('d') if ctrl => self.mutate(|s| s.page(1)),
            KeyCode::Char('d') => {
                let armed = {
                    let mut s = self.state.lock();
                    let was = s.pending_d;
                    s.pending_d = !was;
                    was
                };
                if armed {
                    self.delete_comment();
                }
                self.window.invalidate();
            }
            KeyCode::Char('/') => self.start_find(),
            KeyCode::Char('o') | KeyCode::Char('\t') | KeyCode::Tab => self.toggle_fold(),
            KeyCode::Char(' ') | KeyCode::Char('v') => self.toggle_select(),
            KeyCode::Char('\r') | KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) => {
                self.send_comments()
            }
            KeyCode::Char('\r') | KeyCode::Enter => {
                let on_header = {
                    let s = self.state.lock();
                    s.rows.get(s.cursor).is_some_and(|r| r.kind == RowKind::FileHeader)
                };
                if on_header {
                    self.toggle_fold();
                } else if !self.edit_if_comment() {
                    self.send_selection();
                }
            }
            KeyCode::Char('b') => self.cycle_diff_mode(),
            KeyCode::Char('r') => self.recompute(false),
            KeyCode::Char('g') => self.mutate(|s| s.to_top()),
            KeyCode::Char('G') => self.mutate(|s| s.to_bottom()),
            KeyCode::Char('u') if ctrl => self.mutate(|s| s.page(-1)),
            KeyCode::PageDown => self.mutate(|s| s.page(1)),
            KeyCode::PageUp => self.mutate(|s| s.page(-1)),
            KeyCode::Char('q') | KeyCode::Char('\x1b') => self.request_close(),
            _ => {}
        }
        Ok(())
    }

    fn perform_assignment(&self, assignment: &KeyAssignment) -> PerformAssignmentResult {
        match assignment {
            KeyAssignment::ReviewMode(action) => {
                self.apply_action(action);
                PerformAssignmentResult::Handled
            }
            _ => PerformAssignmentResult::Unhandled,
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        match (event.button, event.kind) {
            (MouseButton::WheelUp(n), _) => self.mutate(|s| s.move_cursor(-(n as isize))),
            (MouseButton::WheelDown(n), _) => self.mutate(|s| s.move_cursor(n as isize)),
            (MouseButton::Left, MouseEventKind::Press) => {
                let doc = {
                    let s = self.state.lock();
                    s.scroll + event.y.max(0) as usize
                };
                let now = Instant::now();
                let double = {
                    let mut s = self.state.lock();
                    let d = matches!(s.last_click, Some((r, t))
                        if r == doc && now.duration_since(t) < Duration::from_millis(400));
                    s.last_click = Some((doc, now));
                    d
                };
                let annotatable = {
                    let s = self.state.lock();
                    s.rows.get(doc).is_some_and(|r| r.anchor.is_some())
                };
                if double && annotatable {
                    self.mutate(|s| {
                        s.cursor = doc;
                        s.select_start = None;
                    });
                    self.start_edit(true);
                    return Ok(());
                }
                let mut to_load = None;
                self.mutate(|s| {
                    if doc >= s.rows.len() {
                        return;
                    }
                    s.cursor = doc;
                    s.select_start = None;
                    if s.rows[doc].kind == RowKind::FileHeader {
                        if let Some(file) = s.rows[doc].file.clone() {
                            to_load = s.set_collapsed_toggle(&file);
                            s.drag_anchor = None;
                            s.rebuild_rows();
                        }
                    } else {
                        s.drag_anchor = Some(doc);
                    }
                });
                if let Some((path, status)) = to_load {
                    if let Some(arc) = self.weak.lock().upgrade() {
                        ReviewPane::spawn_file_load(arc, path, status);
                    }
                }
            }
            (MouseButton::Left, MouseEventKind::Move) => {
                self.mutate(|s| {
                    if let Some(anchor) = s.drag_anchor {
                        let doc = (s.scroll + event.y.max(0) as usize).min(s.rows.len().saturating_sub(1));
                        s.cursor = doc;
                        s.select_start = Some(anchor);
                        s.ensure_visible();
                    }
                });
            }
            (MouseButton::Left, MouseEventKind::Release) => {
                self.mutate(|s| s.drag_anchor = None);
            }
            _ => {}
        }
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.state.lock().dead
    }

    fn palette(&self) -> ColorPalette {
        config::configuration().resolved_palette.clone().into()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn can_close_without_prompting(&self, _reason: mux::pane::CloseReason) -> bool {
        true
    }

    fn is_mouse_grabbed(&self) -> bool {
        true
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        let state = self.state.lock();
        if !state.host.is_local() {
            return None;
        }
        Url::from_directory_path(&state.repo_root).ok()
    }

    fn focus_changed(&self, focused: bool) {
        if !focused {
            return;
        }
        let should = {
            let mut s = self.state.lock();
            if !matches!(s.status, LoadStatus::Ready) || s.editing.is_some() || s.find.is_some() {
                return;
            }
            let ok = s
                .last_refresh
                .map_or(true, |t| Instant::now().duration_since(t) > Duration::from_millis(300));
            if ok {
                s.last_refresh = Some(Instant::now());
            }
            ok
        };
        if should {
            self.recompute(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_review::{DiffHunk, DiffLine, FileDiff, GitDiffData};

    fn dline(t: DiffLineType, old: Option<usize>, new: Option<usize>, s: &str) -> DiffLine {
        DiffLine {
            line_type: t,
            old_line_number: old,
            new_line_number: new,
            text: s.to_string(),
            no_trailing_newline: false,
        }
    }

    fn kinds(rows: &[RenderRow]) -> Vec<RowKind> {
        rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn modified_file_rows() {
        let hunk = DiffHunk {
            old_start_line: 1,
            old_line_count: 2,
            new_start_line: 1,
            new_line_count: 2,
            lines: vec![
                dline(DiffLineType::Context, Some(1), Some(1), "ctx"),
                dline(DiffLineType::Delete, Some(2), None, "old"),
                dline(DiffLineType::Add, None, Some(2), "new"),
            ],
        };
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "src/foo.rs".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 1,
            }],
            total_additions: 1,
            total_deletions: 1,
        };

        let rows = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert_eq!(rows[0].kind, RowKind::FileHeader);
        assert!(rows[0].text.contains("src/foo.rs"));
        assert!(rows[0].text.contains("+1 -1"));
        assert_eq!(rows[1].kind, RowKind::HunkHeader);

        let ks = kinds(&rows);
        assert!(ks.contains(&RowKind::Add));
        assert!(ks.contains(&RowKind::Delete));
        assert!(ks.contains(&RowKind::Context));

        let add = rows.iter().find(|r| r.kind == RowKind::Add).unwrap();
        assert!(add.text.contains("+new"));
        assert!(add.text.contains('2'));
        let del = rows.iter().find(|r| r.kind == RowKind::Delete).unwrap();
        assert!(del.text.contains("-old"));
    }

    #[test]
    fn binary_and_empty_rows() {
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "img.png".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![],
                is_binary: true,
                oversized: false,
                additions: 0,
                deletions: 0,
            }],
            total_additions: 0,
            total_deletions: 0,
        };
        let rows = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Summary && r.text.contains("binary")));

        let empty = build_rows(&GitDiffData::default(), &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].kind, RowKind::Info);
    }

    #[test]
    fn oversized_summary_row() {
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "big.rs".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![],
                is_binary: false,
                oversized: true,
                additions: 9000,
                deletions: 10,
            }],
            total_additions: 9000,
            total_deletions: 10,
        };
        let rows = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Summary && r.text.contains("large diff")));
    }

    #[test]
    fn annotation_rows() {
        let hunk = DiffHunk {
            old_start_line: 1,
            old_line_count: 0,
            new_start_line: 1,
            new_line_count: 1,
            lines: vec![dline(DiffLineType::Add, None, Some(1), "hello")],
        };
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "a.rs".to_string(),
                status: GitFileStatus::New,
                hunks: vec![hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 0,
            }],
            total_additions: 1,
            total_deletions: 0,
        };
        let mut ann = HashMap::new();
        ann.insert(
            LineAnchor {
                file: "a.rs".to_string(),
                side: Side::New,
                line: 1,
            },
            cmt("needs work"),
        );

        let rows = build_rows(&data, &ann, &HashSet::new(), &HashMap::new(), None);
        let note = rows.iter().find(|r| r.kind == RowKind::Note).unwrap();
        assert!(note.text.contains("needs work"));
        let add = rows.iter().find(|r| r.kind == RowKind::Add).unwrap();
        assert!(add.text.starts_with('●'));

        let plain = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert!(plain.iter().all(|r| r.kind != RowKind::Note));
    }

    #[test]
    fn send_payload_includes_file_lines_and_notes() {
        let hunk = DiffHunk {
            old_start_line: 1,
            old_line_count: 1,
            new_start_line: 1,
            new_line_count: 1,
            lines: vec![
                dline(DiffLineType::Delete, Some(1), None, "old line"),
                dline(DiffLineType::Add, None, Some(1), "new line"),
            ],
        };
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "src/a.rs".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 1,
            }],
            total_additions: 1,
            total_deletions: 1,
        };
        let mut ann = HashMap::new();
        ann.insert(
            LineAnchor {
                file: "src/a.rs".to_string(),
                side: Side::New,
                line: 1,
            },
            cmt("use the new api"),
        );
        let rows = build_rows(&data, &ann, &HashSet::new(), &HashMap::new(), None);
        let payload = build_send_payload(&rows, &ann, 0, rows.len() - 1);

        assert!(payload.contains("src/a.rs"));
        assert!(payload.contains("-old line"));
        assert!(payload.contains("+new line"));
        assert!(payload.contains("# note: use the new api"));
    }

    #[test]
    fn folded_file_hides_body() {
        let hunk = DiffHunk {
            old_start_line: 1,
            old_line_count: 1,
            new_start_line: 1,
            new_line_count: 1,
            lines: vec![dline(DiffLineType::Add, None, Some(1), "x")],
        };
        let data = GitDiffData {
            files: vec![FileDiff {
                file_path: "src/foo.rs".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 0,
            }],
            total_additions: 1,
            total_deletions: 0,
        };

        let expanded = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        assert!(expanded.iter().any(|r| r.kind == RowKind::Add));
        assert!(expanded
            .iter()
            .find(|r| r.kind == RowKind::FileHeader)
            .unwrap()
            .text
            .starts_with('▾'));

        let mut collapsed = HashSet::new();
        collapsed.insert("src/foo.rs".to_string());
        let folded = build_rows(&data, &HashMap::new(), &collapsed, &HashMap::new(), None);
        assert!(folded.iter().all(|r| r.kind == RowKind::FileHeader));
        assert!(folded[0].text.starts_with('▸'));
    }

    fn one_add_file() -> GitDiffData {
        let hunk = DiffHunk {
            old_start_line: 0,
            old_line_count: 0,
            new_start_line: 1,
            new_line_count: 1,
            lines: vec![dline(DiffLineType::Add, None, Some(1), "hello")],
        };
        GitDiffData {
            files: vec![FileDiff {
                file_path: "a.rs".to_string(),
                status: GitFileStatus::New,
                hunks: vec![hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 0,
            }],
            total_additions: 1,
            total_deletions: 0,
        }
    }

    fn anchor_a1() -> LineAnchor {
        LineAnchor {
            file: "a.rs".to_string(),
            side: Side::New,
            line: 1,
        }
    }

    fn cmt(body: &str) -> Comment {
        Comment {
            body: body.to_string(),
            line_text: String::new(),
        }
    }

    #[test]
    fn editstate_multiline_ops() {
        let mut e = EditState::new(anchor_a1(), String::new(), None);
        for c in "abc".chars() {
            e.insert_char(c);
        }
        assert_eq!(e.joined(), "abc");
        e.move_h(-1);
        e.insert_char('X');
        assert_eq!(e.lines[0], "abXc");
        e.newline();
        assert_eq!(e.lines.len(), 2);
        assert_eq!(e.lines[1], "c");
        e.backspace();
        assert_eq!(e.lines.len(), 1);
        assert_eq!(e.lines[0], "abXc");
    }

    #[test]
    fn inline_editor_renders_edit_rows() {
        let data = one_add_file();
        let mut edit = EditState::new(anchor_a1(), String::new(), None);
        for c in "hi".chars() {
            edit.insert_char(c);
        }
        edit.newline();
        edit.insert_char('x');

        let rows = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), Some(&edit));
        let edits: Vec<_> = rows.iter().filter(|r| r.kind == RowKind::NoteEdit).collect();
        assert_eq!(edits.len(), 2);
        assert!(edits[0].text.ends_with("hi"));
        assert!(edits[1].text.ends_with('x'));
        assert!(rows.iter().all(|r| r.kind != RowKind::Note));
    }

    #[test]
    fn comment_payload_is_file_line_grouped() {
        let mut ann = HashMap::new();
        ann.insert(
            LineAnchor {
                file: "src/b.rs".to_string(),
                side: Side::New,
                line: 20,
            },
            cmt("second file"),
        );
        ann.insert(
            LineAnchor {
                file: "src/a.rs".to_string(),
                side: Side::New,
                line: 5,
            },
            cmt("first\nsecond"),
        );
        let out = build_comment_payload(&ann);
        let a_pos = out.find("src/a.rs:5: first").expect("a.rs entry");
        let b_pos = out.find("src/b.rs:20: second file").expect("b.rs entry");
        assert!(a_pos < b_pos);
        assert!(out.contains("    second"));
    }

    #[test]
    fn commented_file_header_is_flagged() {
        let data = one_add_file();
        let mut ann = HashMap::new();
        ann.insert(anchor_a1(), cmt("note"));

        let rows = build_rows(&data, &ann, &HashSet::new(), &HashMap::new(), None);
        let header = rows.iter().find(|r| r.kind == RowKind::FileHeader).unwrap();
        assert!(header.commented);
        assert!(header.text.contains('💬'));

        let plain = build_rows(&data, &HashMap::new(), &HashSet::new(), &HashMap::new(), None);
        let h2 = plain.iter().find(|r| r.kind == RowKind::FileHeader).unwrap();
        assert!(!h2.commented);
        assert!(!h2.text.contains('💬'));
    }

    #[test]
    fn reanchor_relocates_comment_by_content() {
        let old_hunk = DiffHunk {
            old_start_line: 10,
            old_line_count: 0,
            new_start_line: 10,
            new_line_count: 1,
            lines: vec![dline(DiffLineType::Add, None, Some(10), "let answer = 42;")],
        };
        let old_data = GitDiffData {
            files: vec![FileDiff {
                file_path: "x.rs".to_string(),
                status: GitFileStatus::Modified,
                hunks: vec![old_hunk],
                is_binary: false,
                oversized: false,
                additions: 1,
                deletions: 0,
            }],
            total_additions: 1,
            total_deletions: 0,
        };
        let old_anchor = LineAnchor {
            file: "x.rs".to_string(),
            side: Side::New,
            line: 10,
        };
        let comment = Comment {
            body: "magic".to_string(),
            line_text: "let answer = 42;".to_string(),
        };

        let mut new_data = old_data.clone();
        new_data.files[0].hunks[0].lines[0].new_line_number = Some(25);
        new_data.files[0].hunks[0].new_start_line = 25;

        let relocated = find_best_anchor(&new_data, &old_anchor, &comment.line_text).unwrap();
        assert_eq!(relocated.line, 25);
        assert_eq!(relocated.file, "x.rs");

        let gone = find_best_anchor(&new_data, &old_anchor, "totally different text");
        assert!(gone.is_none());
    }

    #[test]
    fn orphaned_comment_renders_as_outdated() {
        let data = one_add_file();
        let mut ann = HashMap::new();
        ann.insert(
            LineAnchor {
                file: "a.rs".to_string(),
                side: Side::New,
                line: 999,
            },
            cmt("stale note"),
        );
        let rows = build_rows(&data, &ann, &HashSet::new(), &HashMap::new(), None);
        assert!(rows
            .iter()
            .any(|r| r.text.contains("(outdated)") && r.text.contains("stale note")));
    }

    #[test]
    fn saved_multiline_note_renders_two_rows() {
        let data = one_add_file();
        let mut ann = HashMap::new();
        ann.insert(anchor_a1(), cmt("line one\nline two"));
        let rows = build_rows(&data, &ann, &HashSet::new(), &HashMap::new(), None);
        let notes: Vec<_> = rows.iter().filter(|r| r.kind == RowKind::Note).collect();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].text.contains("line one"));
        assert!(notes[1].text.contains("line two"));
    }
}
