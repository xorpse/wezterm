use git_review::{DiffLine, DiffLineType, FileDiff, Side};
use std::collections::HashMap;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use termwiz::color::SrgbaTuple;

const MAX_LINES: usize = 20_000;
const DARK_THEME: &str = "base16-ocean.dark";
const LIGHT_THEME: &str = "InspiredGitHub";
const TINT_MIX: f32 = 0.18;

#[derive(Clone)]
pub struct Span {
    pub len: usize,
    pub color: Option<SrgbaTuple>,
}

impl Span {
    pub fn plain(len: usize) -> Self {
        Self { len, color: None }
    }
}

pub struct FileHighlight {
    lines: HashMap<(Side, usize), Vec<Span>>,
}

impl FileHighlight {
    pub fn spans(&self, line: &DiffLine) -> &[Span] {
        match key_for(line) {
            Some(key) => self.lines.get(&key).map_or(&[], Vec::as_slice),
            None => &[],
        }
    }
}

fn key_for(line: &DiffLine) -> Option<(Side, usize)> {
    match line.line_type {
        DiffLineType::Delete => line.old_line_number.map(|number| (Side::Old, number)),
        DiffLineType::Add | DiffLineType::Context => {
            line.new_line_number.map(|number| (Side::New, number))
        }
    }
}

#[derive(Clone, Copy)]
pub struct DiffTints {
    pub add: SrgbaTuple,
    pub delete: SrgbaTuple,
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme(dark: bool) -> &'static Theme {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let name = if dark { DARK_THEME } else { LIGHT_THEME };
    themes
        .themes
        .get(name)
        .expect("syntect ships the default themes")
}

fn to_srgba(color: syntect::highlighting::Color) -> SrgbaTuple {
    SrgbaTuple::from((color.r, color.g, color.b))
}

fn mix(base: SrgbaTuple, tint: SrgbaTuple, amount: f32) -> SrgbaTuple {
    SrgbaTuple(
        base.0 + (tint.0 - base.0) * amount,
        base.1 + (tint.1 - base.1) * amount,
        base.2 + (tint.2 - base.2) * amount,
        1.0,
    )
}

pub fn tints(dark: bool) -> DiffTints {
    let background = theme(dark).settings.background.map_or_else(
        || {
            if dark {
                SrgbaTuple(0.0, 0.0, 0.0, 1.0)
            } else {
                SrgbaTuple(1.0, 1.0, 1.0, 1.0)
            }
        },
        to_srgba,
    );
    DiffTints {
        add: mix(background, SrgbaTuple(0.18, 0.8, 0.35, 1.0), TINT_MIX),
        delete: mix(background, SrgbaTuple(0.9, 0.25, 0.3, 1.0), TINT_MIX),
    }
}

fn spans_for(styled: &[(syntect::highlighting::Style, &str)]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (style, text) in styled {
        let len = text.trim_end_matches('\n').chars().count();
        if len == 0 {
            continue;
        }
        let color = to_srgba(style.foreground);
        match spans.last_mut() {
            Some(last) if last.color == Some(color) => last.len += len,
            _ => spans.push(Span {
                len,
                color: Some(color),
            }),
        }
    }
    spans
}

fn highlight_document(
    text: &str,
    syntax: &syntect::parsing::SyntaxReference,
    dark: bool,
) -> Vec<Vec<Span>> {
    let mut highlighter = HighlightLines::new(syntax, theme(dark));
    LinesWithEndings::from(text)
        .map(|line| {
            highlighter
                .highlight_line(line, syntaxes())
                .map_or_else(|_| Vec::new(), |styled| spans_for(&styled))
        })
        .collect()
}

pub fn highlight_file(file: &FileDiff, dark: bool) -> FileHighlight {
    let empty = FileHighlight {
        lines: HashMap::new(),
    };
    let path = std::path::Path::new(&file.file_path);
    let Some(syntax) = path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(|ext| syntaxes().find_syntax_by_extension(ext))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| syntaxes().find_syntax_by_token(name))
        })
    else {
        return empty;
    };

    let mut old = Document::default();
    let mut new = Document::default();
    for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
        let key = key_for(line);
        match line.line_type {
            DiffLineType::Add => new.push(line, key),
            DiffLineType::Delete => old.push(line, key),
            DiffLineType::Context => {
                old.push(line, None);
                new.push(line, key);
            }
        }
    }
    if old.keys.len() + new.keys.len() > MAX_LINES {
        return empty;
    }

    let mut lines = HashMap::new();
    for document in [old, new] {
        let spans = highlight_document(&document.text, syntax, dark);
        for (key, spans) in document.keys.into_iter().zip(spans) {
            if let Some(key) = key {
                lines.insert(key, spans);
            }
        }
    }
    FileHighlight { lines }
}

#[derive(Default)]
struct Document {
    text: String,
    keys: Vec<Option<(Side, usize)>>,
}

impl Document {
    fn push(&mut self, line: &DiffLine, key: Option<(Side, usize)>) {
        self.text.push_str(&line.text);
        self.text.push('\n');
        self.keys.push(key);
    }
}

pub fn slice(spans: &[Span], start: usize, len: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut consumed = 0usize;
    let mut taken = 0usize;
    for span in spans {
        let span_end = consumed + span.len;
        if span_end > start && taken < len {
            let from = start.max(consumed) - consumed;
            let take = (span.len - from).min(len - taken);
            out.push(Span {
                len: take,
                color: span.color,
            });
            taken += take;
        }
        consumed = span_end;
        if taken >= len {
            break;
        }
    }
    if taken < len {
        out.push(Span::plain(len - taken));
    }
    out
}
