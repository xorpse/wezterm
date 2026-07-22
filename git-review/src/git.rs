use crate::mode::DiffMode;
use crate::model::{DiffHunk, DiffLine, DiffLineType, DiffLimits, FileDiff, GitDiffData, GitFileStatus};
use crate::parse::parse_bulk_diff;
use anyhow::{anyhow, Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

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
    let mut command = match host {
        Host::Local => {
            let mut c = Command::new("git");
            c.arg("-C").arg(repo).args(args);
            c
        }
        Host::Ssh(target) => {
            let mut remote = format!("git -C {}", sh_quote(repo));
            for a in args {
                remote.push(' ');
                remote.push_str(&sh_quote(a));
            }
            let mut c = Command::new("ssh");
            c.args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=6",
                target,
                "--",
            ])
            .arg(remote);
            c
        }
    };

    let output = run_with_timeout(&mut command, GIT_TIMEOUT)
        .with_context(|| format!("failed to run git {args:?}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct TimedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<TimedOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout_pipe, &mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr_pipe, &mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(anyhow!("git timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(TimedOutput {
        status,
        stdout,
        stderr,
    })
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

pub fn parent_branch(host: &Host, repo: &str) -> Option<String> {
    let upstream = run_git(
        host,
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|name| !name.is_empty());
    if upstream.is_some() {
        return upstream;
    }
    run_git(
        host,
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|name| !name.is_empty())
}

fn diff_args(host: &Host, repo: &str, mode: &DiffMode) -> Result<Vec<String>> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-color".into(),
        "--find-renames".into(),
        "-l1000".into(),
    ];
    match mode {
        DiffMode::WorkingTree => args.push("HEAD".into()),
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

enum FileScan {
    Text { lines: Vec<DiffLine>, count: usize },
    TooLarge,
    Binary,
}

fn scan_untracked_lines(path: &std::path::Path, max_lines: usize, byte_len: u64) -> FileScan {
    if byte_len == 0 {
        return FileScan::Text {
            lines: Vec::new(),
            count: 0,
        };
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return FileScan::Binary,
    };
    let mmap = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(_) => return FileScan::Binary,
    };
    let text = match std::str::from_utf8(&mmap) {
        Ok(t) => t,
        Err(_) => return FileScan::Binary,
    };
    let mut lines = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if i >= max_lines {
            return FileScan::TooLarge;
        }
        lines.push(DiffLine {
            line_type: DiffLineType::Add,
            old_line_number: None,
            new_line_number: Some(i + 1),
            text: l.to_string(),
            no_trailing_newline: false,
        });
    }
    let count = lines.len();
    FileScan::Text { lines, count }
}

fn untracked_stub(path: &str, is_binary: bool) -> FileDiff {
    FileDiff {
        file_path: path.to_string(),
        status: GitFileStatus::Untracked,
        hunks: Vec::new(),
        is_binary,
        oversized: !is_binary,
        additions: 0,
        deletions: 0,
    }
}

pub(crate) fn synth_untracked(
    repo: &str,
    path: &str,
    limits: &DiffLimits,
    retained_total: &mut usize,
) -> FileDiff {
    let full = std::path::Path::new(repo).join(path);

    if *retained_total >= limits.max_total_lines {
        return untracked_stub(path, false);
    }
    let meta = match std::fs::metadata(&full) {
        Ok(m) => m,
        Err(_) => return untracked_stub(path, true),
    };
    if meta.len() > limits.max_file_bytes {
        return untracked_stub(path, false);
    }

    let remaining = limits.max_total_lines.saturating_sub(*retained_total);
    let cap = limits.max_file_lines.min(remaining);
    match scan_untracked_lines(&full, cap, meta.len()) {
        FileScan::Text { lines, count } => {
            *retained_total += count;
            FileDiff {
                file_path: path.to_string(),
                status: GitFileStatus::Untracked,
                hunks: vec![DiffHunk {
                    old_start_line: 0,
                    old_line_count: 0,
                    new_start_line: 1,
                    new_line_count: count,
                    lines,
                }],
                is_binary: false,
                oversized: false,
                additions: count,
                deletions: 0,
            }
        }
        FileScan::TooLarge => untracked_stub(path, false),
        FileScan::Binary => untracked_stub(path, true),
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

    if matches!(mode, DiffMode::WorkingTree | DiffMode::MergeBase(_)) && host.is_local() {
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

pub fn compute_file_diff(
    host: &Host,
    repo: &str,
    mode: &DiffMode,
    path: &str,
    status: &GitFileStatus,
    limits: &DiffLimits,
) -> Result<FileDiff> {
    if matches!(status, GitFileStatus::Untracked) {
        let mut retained_total = 0usize;
        return Ok(synth_untracked(repo, path, limits, &mut retained_total));
    }

    let mut args = diff_args(host, repo, mode)?;
    args.push("--".into());
    args.push(path.to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let patch = run_git(host, repo, &arg_refs)?;

    parse_bulk_diff(&patch, limits)?
        .into_iter()
        .find(|f| f.file_path == path)
        .ok_or_else(|| anyhow!("no diff produced for {path}"))
}
