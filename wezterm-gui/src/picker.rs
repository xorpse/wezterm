use termwiz::cell::CellAttributes;
use termwiz::color::AnsiColor;

use crate::paseo::agent::{attr_bold, attr_bold_fg, attr_default, attr_dim, attr_fg};

#[derive(Clone)]
pub struct PickerEntry<A> {
    pub dot: Option<(&'static str, AnsiColor)>,
    pub indent: bool,
    pub label: String,
    pub detail: Option<String>,
    pub action: A,
}

impl<A> PickerEntry<A> {
    pub fn plain(label: impl Into<String>, action: A) -> PickerEntry<A> {
        PickerEntry {
            dot: None,
            indent: false,
            label: label.into(),
            detail: None,
            action,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> PickerEntry<A> {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Clone)]
pub struct PickerGroup<A> {
    pub label: String,
    pub collapsed: bool,
    pub entries: Vec<PickerEntry<A>>,
}

#[derive(Clone, Copy)]
pub enum PickerRow {
    Header(usize),
    Entry(usize, usize),
}

pub fn visible_rows<A>(groups: &[PickerGroup<A>]) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        rows.push(PickerRow::Header(gi));
        if !group.collapsed {
            for ei in 0..group.entries.len() {
                rows.push(PickerRow::Entry(gi, ei));
            }
        }
    }
    rows
}

pub fn move_selection<A>(groups: &[PickerGroup<A>], selected: usize, delta: isize) -> usize {
    let len = visible_rows(groups).len() as isize;
    if len == 0 {
        0
    } else {
        (selected as isize + delta).rem_euclid(len) as usize
    }
}

pub enum Activation<A> {
    ToggleGroup(usize),
    Action(A),
}

pub fn activate<A: Clone>(groups: &[PickerGroup<A>], selected: usize) -> Option<Activation<A>> {
    match visible_rows(groups).get(selected)? {
        PickerRow::Header(gi) => Some(Activation::ToggleGroup(*gi)),
        PickerRow::Entry(gi, ei) => Some(Activation::Action(
            groups.get(*gi)?.entries.get(*ei)?.action.clone(),
        )),
    }
}

pub enum BrowseLine {
    Plain {
        text: String,
        attrs: CellAttributes,
    },
    Styled {
        segments: Vec<(String, CellAttributes)>,
        primary: CellAttributes,
    },
}

impl BrowseLine {
    pub fn flatten(&self) -> (String, CellAttributes) {
        match self {
            BrowseLine::Plain { text, attrs } => (text.clone(), attrs.clone()),
            BrowseLine::Styled { segments, primary } => (
                segments
                    .iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>(),
                primary.clone(),
            ),
        }
    }
}

pub struct BrowseView {
    pub lines: Vec<BrowseLine>,
    pub selected_line: usize,
    pub count: usize,
}

pub fn browse_view<A>(
    title: &str,
    crumbs: &[String],
    groups: &[PickerGroup<A>],
    selected: usize,
    cols: usize,
) -> BrowseView {
    let rows = visible_rows(groups);
    let count = rows.len();
    let mut lines = Vec::new();
    lines.push(BrowseLine::Plain {
        text: title.to_owned(),
        attrs: attr_bold_fg(AnsiColor::Teal),
    });
    if !crumbs.is_empty() {
        lines.push(BrowseLine::Plain {
            text: truncate_to(&crumbs.join("  \u{203a}  "), cols),
            attrs: attr_dim(),
        });
    }
    lines.push(BrowseLine::Plain {
        text: "\u{2500}".repeat(cols.max(1)),
        attrs: attr_fg(AnsiColor::Grey),
    });
    lines.push(BrowseLine::Plain {
        text: String::new(),
        attrs: attr_default(),
    });
    if groups.is_empty() {
        lines.push(BrowseLine::Plain {
            text: "  nothing here".to_owned(),
            attrs: attr_dim(),
        });
    }
    let mut selected_line = lines.len();
    for (index, row) in rows.iter().enumerate() {
        let active = index == selected;
        if active {
            selected_line = lines.len();
        }
        match row {
            PickerRow::Header(gi) => {
                let group = &groups[*gi];
                let glyph = if group.collapsed {
                    "\u{25b8}"
                } else {
                    "\u{25be}"
                };
                let marker = if active { "\u{276f} " } else { "  " };
                lines.push(BrowseLine::Plain {
                    text: truncate_to(
                        &format!("{marker}{glyph} {}  ({})", group.label, group.entries.len()),
                        cols,
                    ),
                    attrs: attr_bold_fg(AnsiColor::Teal),
                });
            }
            PickerRow::Entry(gi, ei) => {
                let entry = &groups[*gi].entries[*ei];
                let marker: &str = if active { "\u{276f}   " } else { "    " };
                let marker_attr = if active {
                    attr_bold_fg(AnsiColor::Teal)
                } else {
                    attr_dim()
                };
                let name_attr = if active { attr_bold() } else { attr_default() };
                let mut used = marker.chars().count() + if entry.indent { 2 } else { 0 };
                if entry.dot.is_some() {
                    used += 2;
                }
                used += entry.label.chars().count();
                let detail = entry
                    .detail
                    .as_ref()
                    .map(|detail| truncate_to(detail, cols.saturating_sub(used + 5)));
                let mut segments: Vec<(String, CellAttributes)> =
                    vec![(marker.to_owned(), marker_attr)];
                if entry.indent {
                    segments.push(("  ".to_owned(), attr_default()));
                }
                if let Some((glyph, colour)) = entry.dot {
                    segments.push((glyph.to_owned(), attr_bold_fg(colour)));
                    segments.push((" ".to_owned(), attr_default()));
                }
                segments.push((entry.label.clone(), name_attr.clone()));
                if let Some(detail) = detail {
                    segments.push(("  \u{b7}  ".to_owned(), attr_dim()));
                    segments.push((detail, attr_dim()));
                }
                lines.push(BrowseLine::Styled {
                    segments,
                    primary: name_attr,
                });
            }
        }
    }
    BrowseView {
        lines,
        selected_line,
        count,
    }
}

fn truncate_to(text: &str, max: usize) -> String {
    let max = max.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    let mut truncated: String = chars[..max.saturating_sub(1)].iter().collect();
    truncated.push('\u{2026}');
    truncated
}
