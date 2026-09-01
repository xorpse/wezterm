use std::collections::HashMap;

use git_review::{
    count_changes, DiffHunk, DiffLimits, DiffLine, DiffLineType, DiffMode, FileDiff, GitDiffData,
    GitFileStatus,
};
use orca_client::{
    branch_identity, working_tree_identity, BranchCompareSummary, DiffComment, DiffReviewScope,
    GitDiff, OrcaClient,
};
use similar::{ChangeTag, TextDiff};

use super::ComputedDiff;

#[derive(Clone)]
pub struct OrcaFileMeta {
    pub scope: DiffReviewScope,
    pub identity: String,
    pub old_path: Option<String>,
}

#[derive(Clone)]
pub struct OrcaReviewMeta {
    pub worktree_selector: String,
    pub worktree_id: String,
    pub compare: Option<BranchCompareSummary>,
    pub files: HashMap<String, OrcaFileMeta>,
}

pub struct OrcaComputed {
    pub computed: ComputedDiff,
    pub meta: OrcaReviewMeta,
    pub comments: Vec<DiffComment>,
}

struct FileEntry {
    path: String,
    status: String,
    old_path: Option<String>,
    scope: DiffReviewScope,
    identity: String,
}

fn file_status(status: &str, old_path: Option<&str>) -> GitFileStatus {
    match status {
        "added" => GitFileStatus::New,
        "deleted" => GitFileStatus::Deleted,
        "untracked" => GitFileStatus::Untracked,
        "renamed" => GitFileStatus::Renamed {
            old_path: old_path.unwrap_or_default().to_owned(),
        },
        "copied" => GitFileStatus::Copied {
            old_path: old_path.unwrap_or_default().to_owned(),
        },
        _ => GitFileStatus::Modified,
    }
}

fn hunks_from_contents(original: &str, modified: &str) -> Vec<DiffHunk> {
    let diff = TextDiff::from_lines(original, modified);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(3) {
        let Some(first) = group.first() else {
            continue;
        };
        let Some(last) = group.last() else {
            continue;
        };
        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let raw = change.value();
                let no_trailing_newline = !raw.ends_with('\n');
                let stripped = raw.strip_suffix('\n').unwrap_or(raw);
                let text = stripped.strip_suffix('\r').unwrap_or(stripped).to_owned();
                match change.tag() {
                    ChangeTag::Delete => lines.push(DiffLine {
                        line_type: DiffLineType::Delete,
                        old_line_number: change.old_index().map(|index| index + 1),
                        new_line_number: None,
                        text,
                        no_trailing_newline,
                    }),
                    ChangeTag::Insert => lines.push(DiffLine {
                        line_type: DiffLineType::Add,
                        old_line_number: None,
                        new_line_number: change.new_index().map(|index| index + 1),
                        text,
                        no_trailing_newline,
                    }),
                    ChangeTag::Equal => lines.push(DiffLine {
                        line_type: DiffLineType::Context,
                        old_line_number: change.old_index().map(|index| index + 1),
                        new_line_number: change.new_index().map(|index| index + 1),
                        text,
                        no_trailing_newline,
                    }),
                }
            }
        }
        hunks.push(DiffHunk {
            old_start_line: first.old_range().start + 1,
            old_line_count: last.old_range().end - first.old_range().start,
            new_start_line: first.new_range().start + 1,
            new_line_count: last.new_range().end - first.new_range().start,
            lines,
        });
    }
    hunks
}

async fn fetch_file(
    client: &OrcaClient,
    worktree: &str,
    entry: &FileEntry,
    compare: Option<&BranchCompareSummary>,
    limits: &DiffLimits,
) -> anyhow::Result<FileDiff> {
    let status = file_status(&entry.status, entry.old_path.as_deref());
    let diff = match compare {
        Some(summary) => {
            client
                .git_branch_diff(worktree, &entry.path, summary, entry.old_path.as_deref())
                .await?
        }
        None => client.git_diff(worktree, &entry.path, false, true).await?,
    };
    match diff {
        GitDiff::Binary(_) => Ok(FileDiff {
            file_path: entry.path.clone(),
            status,
            hunks: Vec::new(),
            is_binary: true,
            oversized: false,
            additions: 0,
            deletions: 0,
        }),
        GitDiff::Text(text) => {
            let truncated = text.truncated();
            let too_large = text.original_content.len().max(text.modified_content.len())
                > limits.max_file_bytes as usize
                || text
                    .original_content
                    .lines()
                    .count()
                    .max(text.modified_content.lines().count())
                    > limits.max_file_lines;
            if truncated || too_large {
                return Ok(FileDiff {
                    file_path: entry.path.clone(),
                    status,
                    hunks: Vec::new(),
                    is_binary: false,
                    oversized: true,
                    additions: 0,
                    deletions: 0,
                });
            }
            let hunks = hunks_from_contents(&text.original_content, &text.modified_content);
            let (additions, deletions) = count_changes(&hunks);
            Ok(FileDiff {
                file_path: entry.path.clone(),
                status,
                hunks,
                is_binary: false,
                oversized: false,
                additions,
                deletions,
            })
        }
    }
}

pub async fn fetch(
    client: OrcaClient,
    worktree: String,
    mode: DiffMode,
) -> anyhow::Result<OrcaComputed> {
    let record = client.show_worktree(&worktree).await?;

    let (entries, compare) = match &mode {
        DiffMode::WorkingTree => {
            let status = client.git_status(&worktree).await?;
            let mut staged_only: HashMap<String, bool> = HashMap::new();
            for entry in &status.entries {
                let staged = entry.area == "staged";
                staged_only
                    .entry(entry.path.clone())
                    .and_modify(|value| *value &= staged)
                    .or_insert(staged);
            }
            let mut seen = HashMap::new();
            let mut entries = Vec::new();
            for entry in status.entries {
                if seen.insert(entry.path.clone(), ()).is_some() {
                    continue;
                }
                let scope = if staged_only.get(&entry.path).copied().unwrap_or(false) {
                    DiffReviewScope::Staged
                } else {
                    DiffReviewScope::Unstaged
                };
                let identity = working_tree_identity(
                    scope,
                    &entry.area,
                    &entry.status,
                    entry.old_path.as_deref(),
                    &entry.path,
                    entry.added,
                    entry.removed,
                    entry.conflict_status.as_deref(),
                );
                entries.push(FileEntry {
                    path: entry.path,
                    status: entry.status,
                    old_path: entry.old_path,
                    scope,
                    identity,
                });
            }
            (entries, None)
        }
        DiffMode::Branch(base) | DiffMode::MergeBase(base) => {
            let comparison = client.git_branch_compare(&worktree, base).await?;
            if comparison.summary.status != "ready" {
                anyhow::bail!(
                    "orca cannot compare against {base}: {}",
                    comparison
                        .summary
                        .error_message
                        .as_deref()
                        .unwrap_or(&comparison.summary.status)
                );
            }
            let summary = comparison.summary;
            let entries = comparison
                .entries
                .into_iter()
                .map(|entry| {
                    let identity = branch_identity(
                        summary.merge_base.as_deref(),
                        summary.head_oid.as_deref(),
                        &entry.status,
                        entry.old_path.as_deref(),
                        &entry.path,
                        entry.added,
                        entry.removed,
                    );
                    FileEntry {
                        path: entry.path,
                        status: entry.status,
                        old_path: entry.old_path,
                        scope: DiffReviewScope::Branch,
                        identity,
                    }
                })
                .collect();
            (entries, Some(summary))
        }
    };

    let limits = DiffLimits::default();
    let mut data = GitDiffData::default();
    let mut files = HashMap::new();
    for entry in &entries {
        let file = fetch_file(&client, &worktree, entry, compare.as_ref(), &limits).await?;
        files.insert(
            entry.path.clone(),
            OrcaFileMeta {
                scope: entry.scope,
                identity: entry.identity.clone(),
                old_path: entry.old_path.clone(),
            },
        );
        data.files.push(file);
    }
    data.recompute_totals();

    let branch = Some(
        record
            .git
            .branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&record.git.branch)
            .to_owned(),
    )
    .filter(|branch| !branch.is_empty());
    let parent_branch = compare
        .as_ref()
        .map(|summary| summary.base_ref.clone())
        .filter(|base| !base.is_empty());

    Ok(OrcaComputed {
        computed: ComputedDiff {
            repo_root: record.git.path.clone(),
            branch,
            parent_branch,
            subscription: None,
            data,
        },
        meta: OrcaReviewMeta {
            worktree_selector: worktree,
            worktree_id: record.id,
            compare,
            files,
        },
        comments: record.diff_comments,
    })
}

pub async fn fetch_one(
    client: OrcaClient,
    meta: OrcaReviewMeta,
    path: String,
    status: GitFileStatus,
) -> anyhow::Result<FileDiff> {
    let file = meta
        .files
        .get(&path)
        .ok_or_else(|| anyhow::anyhow!("{path} is not part of the current diff"))?;
    let entry = FileEntry {
        path: path.clone(),
        status: match &status {
            GitFileStatus::New => "added",
            GitFileStatus::Deleted => "deleted",
            GitFileStatus::Untracked => "untracked",
            GitFileStatus::Renamed { .. } => "renamed",
            GitFileStatus::Copied { .. } => "copied",
            _ => "modified",
        }
        .to_owned(),
        old_path: file.old_path.clone(),
        scope: file.scope,
        identity: file.identity.clone(),
    };
    fetch_file(
        &client,
        &meta.worktree_selector,
        &entry,
        meta.compare.as_ref(),
        &DiffLimits::on_demand(),
    )
    .await
}
