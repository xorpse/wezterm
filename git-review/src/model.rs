use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineType {
    Context,
    Add,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Old,
    New,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub line_type: DiffLineType,
    pub old_line_number: Option<usize>,
    pub new_line_number: Option<usize>,
    pub text: String,
    pub no_trailing_newline: bool,
}

impl DiffLine {
    pub fn marker(&self) -> char {
        match self.line_type {
            DiffLineType::Add => '+',
            DiffLineType::Delete => '-',
            DiffLineType::Context => ' ',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start_line: usize,
    pub old_line_count: usize,
    pub new_start_line: usize,
    pub new_line_count: usize,
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    pub fn header_text(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start_line, self.old_line_count, self.new_start_line, self.new_line_count
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitFileStatus {
    New,
    Modified,
    Deleted,
    Renamed { old_path: String },
    Copied { old_path: String },
    Untracked,
    Conflicted,
}

impl GitFileStatus {
    pub fn is_new_file(&self) -> bool {
        matches!(self, Self::New | Self::Untracked)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed { .. } => "renamed",
            Self::Copied { .. } => "copied",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub file_path: String,
    pub status: GitFileStatus,
    pub hunks: Vec<DiffHunk>,
    pub is_binary: bool,
    pub oversized: bool,
    pub additions: usize,
    pub deletions: usize,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        self.additions == 0 && self.deletions == 0 && !self.is_binary
    }
}

pub fn count_changes(hunks: &[DiffHunk]) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line.line_type {
                DiffLineType::Add => adds += 1,
                DiffLineType::Delete => dels += 1,
                DiffLineType::Context => {}
            }
        }
    }
    (adds, dels)
}

#[derive(Clone, Debug, Default)]
pub struct GitDiffData {
    pub files: Vec<FileDiff>,
    pub total_additions: usize,
    pub total_deletions: usize,
}

impl GitDiffData {
    pub fn files_changed(&self) -> usize {
        self.files.len()
    }

    pub fn is_dirty(&self) -> bool {
        !self.files.is_empty()
    }

    pub fn recompute_totals(&mut self) {
        self.total_additions = self.files.iter().map(|f| f.additions).sum();
        self.total_deletions = self.files.iter().map(|f| f.deletions).sum();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiffLimits {
    pub max_file_lines: usize,
    pub max_total_lines: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_file_lines: 20_000,
            max_total_lines: 200_000,
        }
    }
}

pub fn hunk_gap(prev: &DiffHunk, next: &DiffHunk) -> usize {
    let prev_end = prev.new_start_line + prev.new_line_count;
    next.new_start_line.saturating_sub(prev_end)
}

pub fn hunk_new_range(hunk: &DiffHunk) -> Range<usize> {
    hunk.new_start_line..(hunk.new_start_line + hunk.new_line_count)
}
