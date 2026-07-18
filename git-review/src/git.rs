use crate::mode::DiffMode;
use crate::model::{DiffHunk, DiffLine, DiffLineType, DiffLimits, FileDiff, GitDiffData, GitFileStatus};
use crate::parse::parse_bulk_diff;
use anyhow::{anyhow, Context, Result};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Local,
    Ssh(String),
}

impl Host {
    pub fn is_local(&self) -> bool {
        matches!(self, Host::Local)
    }
}

fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn run_git(host: &Host, repo: &str, args: &[&str]) -> Result<String> {
    let output = match host {
        Host::Local => Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn git {args:?}"))?,
        Host::Ssh(target) => {
            let mut remote = format!("git -C {}", sh_quote(repo));
            for a in args {
                remote.push(' ');
                remote.push_str(&sh_quote(a));
            }
            Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=6",
                    target,
                    "--",
                ])
                .arg(remote)
                .output()
                .with_context(|| format!("failed to spawn ssh {target}"))?
        }
    };
    if !output.status.success() {
        return Err(anyhow!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn find_repo_root(host: &Host, start_dir: &str) -> Result<String> {
    let out = run_git(host, start_dir, &["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("not inside a git repository: {start_dir}"));
    }
    Ok(trimmed.to_string())
}

pub fn current_branch(host: &Host, repo: &str) -> Option<String> {
    let out = run_git(host, repo, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let name = out.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn diff_args(host: &Host, repo: &str, mode: &DiffMode) -> Result<Vec<String>> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-color".into(),
        "--find-renames".into(),
    ];
    match mode {
        DiffMode::WorkingTree => args.push("HEAD".into()),
        DiffMode::Staged => args.push("--cached".into()),
        DiffMode::Branch(b) => args.push(b.clone()),
        DiffMode::MergeBase(b) => {
            let base = run_git(host, repo, &["merge-base", "HEAD", b])?
                .trim()
                .to_string();
            if base.is_empty() {
                return Err(anyhow!("no merge base between HEAD and {b}"));
            }
            args.push(base);
        }
    }
    Ok(args)
}

fn synth_untracked(repo: &str, path: &str, limits: &DiffLimits, retained_total: &mut usize) -> FileDiff {
    let full = std::path::Path::new(repo).join(path);
    match std::fs::read_to_string(&full) {
        Ok(content) => {
            let text_lines: Vec<&str> = content.lines().collect();
            let count = text_lines.len();
            let lines: Vec<DiffLine> = text_lines
                .iter()
                .enumerate()
                .map(|(i, l)| DiffLine {
                    line_type: DiffLineType::Add,
                    old_line_number: None,
                    new_line_number: Some(i + 1),
                    text: (*l).to_string(),
                    no_trailing_newline: false,
                })
                .collect();
            let hunk = DiffHunk {
                old_start_line: 0,
                old_line_count: 0,
                new_start_line: 1,
                new_line_count: count,
                lines,
            };
            let oversized = count > limits.max_file_lines || *retained_total + count > limits.max_total_lines;
            if !oversized {
                *retained_total += count;
            }
            FileDiff {
                file_path: path.to_string(),
                status: GitFileStatus::Untracked,
                hunks: if oversized { Vec::new() } else { vec![hunk] },
                is_binary: false,
                oversized,
                additions: count,
                deletions: 0,
            }
        }
        Err(_) => FileDiff {
            file_path: path.to_string(),
            status: GitFileStatus::Untracked,
            hunks: Vec::new(),
            is_binary: true,
            oversized: false,
            additions: 0,
            deletions: 0,
        },
    }
}

pub fn compute_diff(host: &Host, repo: &str, mode: &DiffMode, limits: &DiffLimits) -> Result<GitDiffData> {
    let args = diff_args(host, repo, mode)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let patch = run_git(host, repo, &arg_refs)?;

    let mut files = parse_bulk_diff(&patch, limits)?;
    let mut retained_total: usize = files
        .iter()
        .map(|f| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>())
        .sum();

    if matches!(mode, DiffMode::WorkingTree) && host.is_local() {
        let others = run_git(host, repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for path in others.split('\0').filter(|s| !s.is_empty()) {
            files.push(synth_untracked(repo, path, limits, &mut retained_total));
        }
    }

    let mut data = GitDiffData {
        files,
        total_additions: 0,
        total_deletions: 0,
    };
    data.recompute_totals();
    Ok(data)
}
