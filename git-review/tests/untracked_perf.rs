use git_review::{compute_diff, compute_file_diff, DiffLimits, DiffMode, GitFileStatus, Host};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("git-review-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn large_untracked_file_is_withheld_fast_then_loadable() {
    let dir = scratch("untracked");
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("tracked.txt"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let big = "x".repeat(64 * 1024 * 1024);
    std::fs::write(dir.join("big.log"), &big).unwrap();

    let repo = dir.to_str().unwrap();
    let start = Instant::now();
    let data = compute_diff(&Host::Local, repo, &DiffMode::WorkingTree, &DiffLimits::default())
        .expect("compute_diff");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "compute_diff took too long: {elapsed:?} (the 64MB untracked file should not be read)"
    );

    let big_file = data
        .files
        .iter()
        .find(|f| f.file_path == "big.log")
        .expect("big.log present");
    assert!(big_file.oversized, "big untracked file should be withheld");
    assert!(big_file.hunks.is_empty(), "withheld file has no hunks");

    let loaded = compute_file_diff(
        &Host::Local,
        repo,
        &DiffMode::WorkingTree,
        "big.log",
        &GitFileStatus::Untracked,
        &DiffLimits::on_demand(),
    )
    .expect("compute_file_diff");
    assert!(!loaded.oversized, "64MB file loads under on-demand limits");
    assert_eq!(loaded.additions, 1, "single long line");

    let _ = std::fs::remove_dir_all(&dir);
}
