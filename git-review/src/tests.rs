use crate::collapse::calculate_hidden_lines;
use crate::model::{DiffLimits, DiffLineType, GitFileStatus};
use crate::parse::{parse_bulk_diff, parse_unified_diff_header};

const MODIFIED: &str = concat!(
    "diff --git a/foo.txt b/foo.txt\n",
    "index 1111111..2222222 100644\n",
    "--- a/foo.txt\n",
    "+++ b/foo.txt\n",
    "@@ -1,4 +1,4 @@\n",
    " context1\n",
    "-old line\n",
    "+new line\n",
    " context2\n",
    " context3\n",
);

const NEW_FILE: &str = concat!(
    "diff --git a/new.txt b/new.txt\n",
    "new file mode 100644\n",
    "index 0000000..abcdef1\n",
    "--- /dev/null\n",
    "+++ b/new.txt\n",
    "@@ -0,0 +1,2 @@\n",
    "+hello\n",
    "+world\n",
);

const DELETED: &str = concat!(
    "diff --git a/del.txt b/del.txt\n",
    "deleted file mode 100644\n",
    "index abcdef1..0000000\n",
    "--- a/del.txt\n",
    "+++ /dev/null\n",
    "@@ -1 +0,0 @@\n",
    "-gone\n",
);

const RENAMED: &str = concat!(
    "diff --git a/old_name.txt b/new_name.txt\n",
    "similarity index 66%\n",
    "rename from old_name.txt\n",
    "rename to new_name.txt\n",
    "index abcdef1..def4567 100644\n",
    "--- a/old_name.txt\n",
    "+++ b/new_name.txt\n",
    "@@ -1,2 +1,2 @@\n",
    " keep\n",
    "-was\n",
    "+now\n",
);

const BINARY: &str = concat!(
    "diff --git a/img.png b/img.png\n",
    "index abcdef1..def4567 100644\n",
    "Binary files a/img.png and b/img.png differ\n",
);

const NO_NEWLINE: &str = concat!(
    "diff --git a/nonl.txt b/nonl.txt\n",
    "index abcdef1..def4567 100644\n",
    "--- a/nonl.txt\n",
    "+++ b/nonl.txt\n",
    "@@ -1 +1 @@\n",
    "-old\n",
    "\\ No newline at end of file\n",
    "+new\n",
    "\\ No newline at end of file\n",
);

#[test]
fn header_parsing() {
    let h = parse_unified_diff_header("@@ -3,5 +10,7 @@ fn thing()").unwrap();
    assert_eq!(h.old_start_line, 3);
    assert_eq!(h.old_line_count, 5);
    assert_eq!(h.new_start_line, 10);
    assert_eq!(h.new_line_count, 7);

    let single = parse_unified_diff_header("@@ -1 +1 @@").unwrap();
    assert_eq!(single.old_line_count, 1);
    assert_eq!(single.new_line_count, 1);
}

#[test]
fn modified_file() {
    let files = parse_bulk_diff(MODIFIED, &DiffLimits::default()).unwrap();
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(f.file_path, "foo.txt");
    assert_eq!(f.status, GitFileStatus::Modified);
    assert_eq!(f.additions, 1);
    assert_eq!(f.deletions, 1);
    assert_eq!(f.hunks.len(), 1);

    let lines = &f.hunks[0].lines;
    assert_eq!(lines[0].line_type, DiffLineType::Context);
    assert_eq!(lines[0].old_line_number, Some(1));
    assert_eq!(lines[0].new_line_number, Some(1));
    assert_eq!(lines[1].line_type, DiffLineType::Delete);
    assert_eq!(lines[1].old_line_number, Some(2));
    assert_eq!(lines[1].new_line_number, None);
    assert_eq!(lines[2].line_type, DiffLineType::Add);
    assert_eq!(lines[2].new_line_number, Some(2));
    assert_eq!(lines[2].text, "new line");
}

#[test]
fn new_and_deleted() {
    let new = &parse_bulk_diff(NEW_FILE, &DiffLimits::default()).unwrap()[0];
    assert_eq!(new.file_path, "new.txt");
    assert_eq!(new.status, GitFileStatus::New);
    assert_eq!(new.additions, 2);
    assert_eq!(new.deletions, 0);

    let del = &parse_bulk_diff(DELETED, &DiffLimits::default()).unwrap()[0];
    assert_eq!(del.file_path, "del.txt");
    assert_eq!(del.status, GitFileStatus::Deleted);
    assert_eq!(del.additions, 0);
    assert_eq!(del.deletions, 1);
}

#[test]
fn rename_detection() {
    let f = &parse_bulk_diff(RENAMED, &DiffLimits::default()).unwrap()[0];
    assert_eq!(f.file_path, "new_name.txt");
    assert_eq!(
        f.status,
        GitFileStatus::Renamed {
            old_path: "old_name.txt".to_string()
        }
    );
    assert_eq!(f.additions, 1);
    assert_eq!(f.deletions, 1);
}

#[test]
fn binary_file() {
    let f = &parse_bulk_diff(BINARY, &DiffLimits::default()).unwrap()[0];
    assert_eq!(f.file_path, "img.png");
    assert!(f.is_binary);
    assert!(f.hunks.is_empty());
    assert!(!f.oversized);
}

#[test]
fn no_newline_marker() {
    let f = &parse_bulk_diff(NO_NEWLINE, &DiffLimits::default()).unwrap()[0];
    let lines = &f.hunks[0].lines;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].line_type, DiffLineType::Delete);
    assert!(lines[0].no_trailing_newline);
    assert_eq!(lines[1].line_type, DiffLineType::Add);
    assert!(lines[1].no_trailing_newline);
}

#[test]
fn multi_file() {
    let combined = format!("{MODIFIED}{NEW_FILE}{BINARY}");
    let files = parse_bulk_diff(&combined, &DiffLimits::default()).unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].file_path, "foo.txt");
    assert_eq!(files[1].file_path, "new.txt");
    assert_eq!(files[2].file_path, "img.png");
}

#[test]
fn oversized_drops_hunks() {
    let limits = DiffLimits {
        max_file_lines: 1,
        max_total_lines: 1_000,
        ..DiffLimits::default()
    };
    let f = &parse_bulk_diff(MODIFIED, &limits).unwrap()[0];
    assert!(f.oversized);
    assert!(f.hunks.is_empty());
    assert_eq!(f.additions, 1);
    assert_eq!(f.deletions, 1);
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("git-review-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn untracked_small_file_reads() {
    let dir = temp_repo("small");
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let mut retained = 0;
    let f = crate::git::synth_untracked(dir.to_str().unwrap(), "a.txt", &DiffLimits::default(), &mut retained);
    assert!(!f.oversized);
    assert_eq!(f.additions, 3);
    assert_eq!(retained, 3);
    assert_eq!(f.hunks[0].lines.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn untracked_over_byte_cap_is_withheld() {
    let dir = temp_repo("bytes");
    std::fs::write(dir.join("big.dat"), vec![b'x'; 4096]).unwrap();
    let limits = DiffLimits {
        max_file_bytes: 1024,
        ..DiffLimits::default()
    };
    let mut retained = 0;
    let f = crate::git::synth_untracked(dir.to_str().unwrap(), "big.dat", &limits, &mut retained);
    assert!(f.oversized);
    assert!(f.hunks.is_empty());
    assert_eq!(retained, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn untracked_withheld_once_total_budget_hit() {
    let dir = temp_repo("budget");
    std::fs::write(dir.join("a.txt"), "line\n").unwrap();
    let limits = DiffLimits {
        max_total_lines: 10,
        ..DiffLimits::default()
    };
    let mut retained = 10;
    let f = crate::git::synth_untracked(dir.to_str().unwrap(), "a.txt", &limits, &mut retained);
    assert!(f.oversized);
    assert!(f.hunks.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn untracked_over_line_cap_is_withheld() {
    let dir = temp_repo("lines");
    let body: String = (0..100).map(|i| format!("line{i}\n")).collect();
    std::fs::write(dir.join("many.txt"), body).unwrap();
    let limits = DiffLimits {
        max_file_lines: 10,
        ..DiffLimits::default()
    };
    let mut retained = 0;
    let f = crate::git::synth_untracked(dir.to_str().unwrap(), "many.txt", &limits, &mut retained);
    assert!(f.oversized);
    assert!(f.hunks.is_empty());
    let generous = crate::git::synth_untracked(dir.to_str().unwrap(), "many.txt", &DiffLimits::on_demand(), &mut 0);
    assert!(!generous.oversized);
    assert_eq!(generous.additions, 100);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collapse_ranges() {
    let hidden = calculate_hidden_lines(10, &[5], 2);
    assert_eq!(hidden, vec![0..3, 8..10]);

    let none = calculate_hidden_lines(6, &[0, 1, 2, 3, 4, 5], 0);
    assert!(none.is_empty());

    let all = calculate_hidden_lines(4, &[], 3);
    assert_eq!(all, vec![0..4]);
}
