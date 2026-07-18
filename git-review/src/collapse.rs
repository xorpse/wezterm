use std::ops::Range;

pub fn calculate_hidden_lines(
    total_rows: usize,
    anchors: &[usize],
    context: usize,
) -> Vec<Range<usize>> {
    if total_rows == 0 {
        return Vec::new();
    }

    let mut visible = vec![false; total_rows];
    for &anchor in anchors {
        if anchor >= total_rows {
            continue;
        }
        let lo = anchor.saturating_sub(context);
        let hi = (anchor + context + 1).min(total_rows);
        for slot in &mut visible[lo..hi] {
            *slot = true;
        }
    }

    let mut hidden = Vec::new();
    let mut run_start = None;
    for (idx, &vis) in visible.iter().enumerate() {
        match (vis, run_start) {
            (false, None) => run_start = Some(idx),
            (true, Some(start)) => {
                hidden.push(start..idx);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        hidden.push(start..total_rows);
    }
    hidden
}
