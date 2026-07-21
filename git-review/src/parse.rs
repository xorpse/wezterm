use crate::model::{count_changes, DiffHunk, DiffLine, DiffLineType, DiffLimits, FileDiff, GitFileStatus};
use anyhow::{anyhow, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnifiedDiffHeader {
    pub old_start_line: usize,
    pub old_line_count: usize,
    pub new_start_line: usize,
    pub new_line_count: usize,
}

pub fn parse_range(range_str: &str) -> Result<(usize, usize)> {
    if let Some(comma_pos) = range_str.find(',') {
        let start: usize = range_str[..comma_pos]
            .parse()
            .map_err(|_| anyhow!("invalid range start: {range_str}"))?;
        let count: usize = range_str[comma_pos + 1..]
            .parse()
            .map_err(|_| anyhow!("invalid range count: {range_str}"))?;
        Ok((start, count))
    } else {
        let start: usize = range_str
            .parse()
            .map_err(|_| anyhow!("invalid range: {range_str}"))?;
        Ok((start, 1))
    }
}

pub fn parse_unified_diff_header(header_line: &str) -> Result<UnifiedDiffHeader> {
    if !header_line.starts_with("@@") {
        return Err(anyhow!("invalid unified diff header: {header_line}"));
    }

    let header_parts: Vec<&str> = header_line.split_whitespace().take(3).collect();
    if header_parts.len() < 3 {
        return Err(anyhow!("invalid unified diff header format: {header_line}"));
    }

    let old_range = header_parts[1]
        .strip_prefix('-')
        .ok_or_else(|| anyhow!("invalid old range: {header_line}"))?;
    let new_range = header_parts[2]
        .strip_prefix('+')
        .ok_or_else(|| anyhow!("invalid new range: {header_line}"))?;

    let (old_start_line, old_line_count) = parse_range(old_range)?;
    let (new_start_line, new_line_count) = parse_range(new_range)?;

    Ok(UnifiedDiffHeader {
        old_start_line,
        old_line_count,
        new_start_line,
        new_line_count,
    })
}

pub fn parse_hunks(section: &[&str]) -> Result<Vec<DiffHunk>> {
    let mut hunks = Vec::new();
    let mut i = 0;

    while i < section.len() {
        let line = section[i];
        if !line.starts_with("@@") {
            i += 1;
            continue;
        }

        let header = parse_unified_diff_header(line)?;
        let mut hunk_lines = Vec::new();
        let mut old_line = header.old_start_line;
        let mut new_line = header.new_start_line;
        i += 1;

        while i < section.len() && !section[i].starts_with("@@") {
            let content_line = section[i];
            i += 1;

            if content_line.starts_with('\\') {
                if let Some(last) = hunk_lines.last_mut() {
                    let last: &mut DiffLine = last;
                    last.no_trailing_newline = true;
                }
                continue;
            }

            let (line_type, old_num, new_num) = match content_line.chars().next() {
                Some('+') => {
                    let num = new_line;
                    new_line += 1;
                    (DiffLineType::Add, None, Some(num))
                }
                Some('-') => {
                    let num = old_line;
                    old_line += 1;
                    (DiffLineType::Delete, Some(num), None)
                }
                Some(' ') => {
                    let o = old_line;
                    let n = new_line;
                    old_line += 1;
                    new_line += 1;
                    (DiffLineType::Context, Some(o), Some(n))
                }
                _ => continue,
            };

            let text = if content_line.len() > 1 {
                content_line[1..].to_string()
            } else {
                String::new()
            };

            hunk_lines.push(DiffLine {
                line_type,
                old_line_number: old_num,
                new_line_number: new_num,
                text,
                no_trailing_newline: false,
            });
        }

        hunks.push(DiffHunk {
            old_start_line: header.old_start_line,
            old_line_count: header.old_line_count,
            new_start_line: header.new_start_line,
            new_line_count: header.new_line_count,
            lines: hunk_lines,
        });
    }

    Ok(hunks)
}

fn strip_ab_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn section_path(section: &[&str]) -> Option<String> {
    let mut new_path = None;
    let mut old_path = None;
    for &line in section {
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest != "/dev/null" {
                new_path = Some(strip_ab_prefix(rest).to_string());
            }
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if rest != "/dev/null" {
                old_path = Some(strip_ab_prefix(rest).to_string());
            }
        } else if line.starts_with("@@") {
            break;
        }
    }
    new_path.or(old_path)
}

fn git_header_path(diff_git_line: &str) -> Option<String> {
    let rest = diff_git_line.strip_prefix("diff --git ")?;
    let b_pos = rest.rfind(" b/")?;
    Some(strip_ab_prefix(&rest[b_pos + 1..]).to_string())
}

fn section_status(section: &[&str]) -> (GitFileStatus, Option<String>) {
    let mut rename_from = None;
    for &line in section {
        if line.starts_with("new file mode") {
            return (GitFileStatus::New, None);
        }
        if line.starts_with("deleted file mode") {
            return (GitFileStatus::Deleted, None);
        }
        if let Some(from) = line.strip_prefix("rename from ") {
            rename_from = Some(from.to_string());
        }
        if line.starts_with("copy from ") {
            let from = line.trim_start_matches("copy from ").to_string();
            return (GitFileStatus::Copied { old_path: from }, None);
        }
        if line.starts_with("@@") {
            break;
        }
    }
    match rename_from {
        Some(old_path) => (GitFileStatus::Renamed { old_path }, None),
        None => (GitFileStatus::Modified, None),
    }
}

fn is_binary_section(section: &[&str]) -> bool {
    section
        .iter()
        .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"))
}

fn build_file_diff(
    path: String,
    status: GitFileStatus,
    is_binary: bool,
    hunks: Vec<DiffHunk>,
    limits: &DiffLimits,
    retained_total: &mut usize,
) -> FileDiff {
    let (additions, deletions) = count_changes(&hunks);
    let file_lines: usize = hunks.iter().map(|h| h.lines.len()).sum();
    let over_budget = *retained_total + file_lines > limits.max_total_lines;
    let oversized = is_binary == false && (file_lines > limits.max_file_lines || over_budget);

    let hunks = if oversized { Vec::new() } else { hunks };
    if !oversized {
        *retained_total += file_lines;
    }

    FileDiff {
        file_path: path,
        status,
        hunks,
        is_binary,
        oversized,
        additions,
        deletions,
    }
}

pub fn parse_bulk_diff(patch: &str, limits: &DiffLimits) -> Result<Vec<FileDiff>> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut file_starts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            file_starts.push(idx);
        }
    }

    let mut files = Vec::new();
    let mut retained_total = 0usize;

    for (n, &start) in file_starts.iter().enumerate() {
        let end = file_starts.get(n + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];

        let path = section_path(section)
            .or_else(|| git_header_path(section[0]))
            .ok_or_else(|| anyhow!("could not determine path for diff section"))?;

        let (status, _) = section_status(section);
        let is_binary = is_binary_section(section);
        let hunks = if is_binary {
            Vec::new()
        } else {
            parse_hunks(section)?
        };

        files.push(build_file_diff(
            path,
            status,
            is_binary,
            hunks,
            limits,
            &mut retained_total,
        ));
    }

    Ok(files)
}
