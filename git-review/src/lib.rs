pub mod collapse;
pub mod git;
pub mod mode;
pub mod model;
pub mod parse;

pub use collapse::calculate_hidden_lines;
pub use git::{
    compute_diff, compute_file_diff, current_branch, find_repo_root, parent_branch, Host,
};
pub use mode::DiffMode;
pub use model::{
    count_changes, hunk_gap, hunk_new_range, DiffHunk, DiffLimits, DiffLine, DiffLineType, FileDiff,
    GitDiffData, GitFileStatus, Side,
};
pub use parse::{parse_bulk_diff, parse_unified_diff_header, UnifiedDiffHeader};

#[cfg(test)]
mod tests;
