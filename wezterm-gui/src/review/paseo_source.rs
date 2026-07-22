use super::ComputedDiff;
use git_review::{
    DiffHunk, DiffLine, DiffLineType, DiffMode, FileDiff, GitDiffData, GitFileStatus,
};
use paseo_client::{DiffFile, PaseoClient};

pub fn paseo_compare(mode: &DiffMode) -> (&'static str, Option<String>) {
    match mode {
        DiffMode::Branch(base) | DiffMode::MergeBase(base) => ("base", Some(base.clone())),
        DiffMode::WorkingTree => ("uncommitted", None),
    }
}

pub fn convert(files: Vec<DiffFile>) -> GitDiffData {
    let mut data = GitDiffData::default();
    for file in files {
        let status = if file.is_new {
            GitFileStatus::New
        } else if file.is_deleted {
            GitFileStatus::Deleted
        } else {
            GitFileStatus::Modified
        };
        let is_binary = file.status.as_deref() == Some("binary");
        let oversized = file.status.as_deref() == Some("too_large");

        let mut hunks = Vec::new();
        if !is_binary && !oversized {
            for hunk in file.hunks {
                let mut lines = Vec::new();
                let mut old_ln = hunk.old_start as usize;
                let mut new_ln = hunk.new_start as usize;
                for line in hunk.lines {
                    match line.r#type.as_str() {
                        "add" => {
                            lines.push(DiffLine {
                                line_type: DiffLineType::Add,
                                old_line_number: None,
                                new_line_number: Some(new_ln),
                                text: line.content,
                                no_trailing_newline: false,
                            });
                            new_ln += 1;
                        }
                        "remove" => {
                            lines.push(DiffLine {
                                line_type: DiffLineType::Delete,
                                old_line_number: Some(old_ln),
                                new_line_number: None,
                                text: line.content,
                                no_trailing_newline: false,
                            });
                            old_ln += 1;
                        }
                        "context" => {
                            lines.push(DiffLine {
                                line_type: DiffLineType::Context,
                                old_line_number: Some(old_ln),
                                new_line_number: Some(new_ln),
                                text: line.content,
                                no_trailing_newline: false,
                            });
                            old_ln += 1;
                            new_ln += 1;
                        }
                        _ => {}
                    }
                }
                hunks.push(DiffHunk {
                    old_start_line: hunk.old_start as usize,
                    old_line_count: hunk.old_count as usize,
                    new_start_line: hunk.new_start as usize,
                    new_line_count: hunk.new_count as usize,
                    lines,
                });
            }
        }

        data.files.push(FileDiff {
            file_path: file.path,
            status,
            hunks,
            is_binary,
            oversized,
            additions: file.additions as usize,
            deletions: file.deletions as usize,
        });
    }
    data.recompute_totals();
    data
}

pub async fn fetch(
    client: PaseoClient,
    cwd: String,
    mode: DiffMode,
) -> anyhow::Result<ComputedDiff> {
    let (mode_str, base_ref) = paseo_compare(&mode);
    let status = client.checkout_status(&cwd).await.ok();
    let repo_root = status
        .as_ref()
        .and_then(|s| s.repo_root.clone())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| cwd.clone());
    let branch = status
        .as_ref()
        .and_then(|s| s.current_branch.clone())
        .filter(|b| !b.is_empty());
    let parent_branch = status
        .as_ref()
        .and_then(|s| s.base_ref.clone())
        .filter(|b| !b.is_empty());

    let diff = client
        .subscribe_checkout_diff(&cwd, mode_str, base_ref.as_deref())
        .await?;
    let _ = client
        .unsubscribe_checkout_diff(&diff.subscription_id)
        .await;

    if let Some(error) = diff.error {
        anyhow::bail!("{}", error.message);
    }

    Ok(ComputedDiff {
        repo_root,
        branch,
        parent_branch,
        data: convert(diff.files),
    })
}
