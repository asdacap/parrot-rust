//! Terminal text metrics, wrapping and Markdown rendering for the inline CLI.
//!
//! Cell widths intentionally follow the source implementation's small, stable
//! width table rather than locale-dependent terminal measurements.

mod final_format;
mod markdown;
mod terminal_line;

pub use final_format::{EnhancedFinalMessageRenderer, FinalFormatError, ToolYamlFormatter};
pub use markdown::MarkdownRenderer;
pub use terminal_line::{TerminalCellStyleSpan, TerminalLine, TerminalStyle};

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_segmentation::UnicodeSegmentation;

/// Source-compatible sanitization and cell-width operations.
pub struct TerminalText;

impl TerminalText {
    pub fn sanitize(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for c in value.chars() {
            match c {
                '\t' => out.push_str("    "),
                '\n' => out.push('\n'),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        out
    }

    pub fn sanitize_line(value: &str) -> String {
        Self::sanitize(value).replace('\n', " ")
    }

    /// Source's custom Unicode terminal-cell width for one scalar.
    pub fn rune_width(c: char) -> usize {
        let n = c as u32;
        if is_combining(n)
            || n == 0x200d
            || (0x1f3fb..=0x1f3ff).contains(&n)
            || (0xfe00..=0xfe0f).contains(&n)
            || (0xe0100..=0xe01ef).contains(&n)
        {
            return 0;
        }
        if (0x1100..=0x115f).contains(&n)
            || matches!(n, 0x2329 | 0x232a)
            || (0x2e80..=0xa4cf).contains(&n)
            || (0xac00..=0xd7a3).contains(&n)
            || (0xf900..=0xfaff).contains(&n)
            || (0xfe10..=0xfe6f).contains(&n)
            || (0xff00..=0xff60).contains(&n)
            || (0x1f300..=0x1faff).contains(&n)
            || (0x20000..=0x3fffd).contains(&n)
        {
            2
        } else {
            1
        }
    }

    pub fn width(value: &str) -> usize {
        graphemes(value).iter().map(|g| grapheme_width(g)).sum()
    }

    pub fn clip(value: &str, width: usize) -> String {
        let mut used = 0;
        let mut out = String::new();
        for g in graphemes(value) {
            let w = grapheme_width(&g);
            if used + w > width {
                break;
            }
            used += w;
            out.push_str(&g);
        }
        out
    }

    pub fn layout(value: &str, width: usize) -> Vec<String> {
        Self::layout_hanging(value, width, "")
    }

    pub fn layout_hanging(value: &str, width: usize, indent: &str) -> Vec<String> {
        let width = width.max(1);
        let mut indent = Self::sanitize(indent).replace('\n', "");
        if Self::width(&indent) >= width {
            indent.clear();
        }
        let iw = Self::width(&indent);
        let mut result = Vec::new();
        let mut row = String::new();
        let mut cells = 0;
        for g in graphemes(value) {
            if g == "\n" {
                result.push(row);
                row = indent.clone();
                cells = iw;
                continue;
            }
            let gw = grapheme_width(&g);
            if cells > 0 && cells + gw > width {
                result.push(row);
                row = indent.clone();
                cells = iw;
            }
            row.push_str(&g);
            cells += gw;
        }
        result.push(row);
        result
    }

    pub fn layout_words(value: &str, width: usize) -> Vec<String> {
        Self::layout_words_hanging(value, width, "")
    }

    pub fn layout_words_hanging(value: &str, width: usize, indent: &str) -> Vec<String> {
        let width = width.max(1);
        let mut indent = Self::sanitize(indent).replace('\n', "");
        if Self::width(&indent) >= width {
            indent.clear();
        }
        let indent_width = Self::width(&indent);
        let content_width = (width - indent_width).max(1);
        let layout = WordLayoutConfig {
            indent: &indent,
            indent_width,
            width,
            content_width,
        };
        let mut rows = Vec::new();
        let mut row = String::new();
        let mut cells = 0;
        let mut row_start = 0;
        let mut pending_space = String::new();
        let input_lines: Vec<&str> = value.split('\n').collect();
        for (line_index, line) in input_lines.iter().enumerate() {
            let mut word = String::new();
            for g in graphemes(line) {
                if g.chars().next().is_some_and(char::is_whitespace) {
                    if !word.is_empty() {
                        append_word(
                            &mut rows,
                            &mut row,
                            &mut cells,
                            &mut row_start,
                            &layout,
                            &word,
                            &pending_space,
                        );
                        word.clear();
                        pending_space.clear();
                    }
                    pending_space.push_str(&g);
                } else {
                    word.push_str(&g);
                }
            }
            if !word.is_empty() {
                append_word(
                    &mut rows,
                    &mut row,
                    &mut cells,
                    &mut row_start,
                    &layout,
                    &word,
                    &pending_space,
                );
                pending_space.clear();
            }
            for g in graphemes(&pending_space) {
                let gw = grapheme_width(&g);
                if cells + gw > width {
                    break;
                }
                row.push_str(&g);
                cells += gw;
            }
            pending_space.clear();
            if line_index + 1 < input_lines.len() {
                rows.push(std::mem::replace(&mut row, indent.clone()));
                cells = indent_width;
                row_start = indent_width;
            }
        }
        rows.push(row);
        rows
    }
}

struct WordLayoutConfig<'a> {
    indent: &'a str,
    indent_width: usize,
    width: usize,
    content_width: usize,
}

fn append_word(
    rows: &mut Vec<String>,
    row: &mut String,
    cells: &mut usize,
    row_start: &mut usize,
    layout: &WordLayoutConfig<'_>,
    word: &str,
    spacing: &str,
) {
    let ww = TerminalText::width(word);
    let sw = TerminalText::width(spacing);
    if *cells + sw + ww <= layout.width {
        row.push_str(spacing);
        row.push_str(word);
        *cells += sw + ww;
        return;
    }
    if ww > layout.content_width {
        if *cells + sw <= layout.width {
            row.push_str(spacing);
            *cells += sw;
        }
        for g in graphemes(word) {
            let gw = grapheme_width(&g);
            if *cells > *row_start && *cells + gw > layout.width {
                rows.push(std::mem::replace(row, layout.indent.to_owned()));
                *cells = layout.indent_width;
                *row_start = layout.indent_width;
            }
            row.push_str(&g);
            *cells += gw;
        }
    } else {
        if *cells > *row_start {
            rows.push(std::mem::replace(row, layout.indent.to_owned()));
            *cells = layout.indent_width;
            *row_start = layout.indent_width;
        }
        row.push_str(word);
        *cells += ww;
    }
}

fn is_combining(n: u32) -> bool {
    char::from_u32(n).is_some_and(|rune| {
        matches!(
            get_general_category(rune),
            GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark
        )
    })
}

fn graphemes(s: &str) -> Vec<String> {
    s.graphemes(true).map(str::to_owned).collect()
}

fn grapheme_width(g: &str) -> usize {
    let chars: Vec<char> = g.chars().collect();
    if chars.len() == 2
        && chars
            .iter()
            .all(|c| (0x1f1e6..=0x1f1ff).contains(&(*c as u32)))
    {
        return 2;
    }
    let sum: usize = chars.iter().copied().map(TerminalText::rune_width).sum();
    if chars.contains(&'\u{200d}') {
        chars
            .iter()
            .copied()
            .map(TerminalText::rune_width)
            .max()
            .unwrap_or(0)
    } else {
        sum
    }
}

/// Small live-stream accumulator: only newline-complete safe regions are
/// promoted; incomplete fences, tables and ordered lists remain preview text until
/// a boundary or commit.
pub struct MarkdownLiveRenderer {
    id: String,
    prefix: String,
    pending: String,
    started: bool,
    width: usize,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MarkdownLiveUpdate {
    pub scrollback: Option<Vec<String>>,
    pub preview_prefix: String,
    pub preview: Vec<String>,
}
impl MarkdownLiveRenderer {
    pub fn new(width: usize) -> Self {
        Self {
            id: String::new(),
            prefix: String::new(),
            pending: String::new(),
            started: false,
            width: width.max(1),
        }
    }
    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(1);
    }
    pub fn append(
        &mut self,
        id: &str,
        prefix: &str,
        fragment: &str,
        color: bool,
    ) -> Result<MarkdownLiveUpdate, &'static str> {
        let id = TerminalText::sanitize(id);
        let prefix = TerminalText::sanitize(prefix);
        if id.is_empty() {
            return Err("The stream message ID is empty.");
        }
        if !self.id.is_empty() && self.id != id {
            return Err("Another assistant message is still streaming.");
        }
        if !self.id.is_empty() && self.prefix != prefix {
            return Err("The stream message prefix changed.");
        }
        let candidate = format!("{}{}", self.pending, TerminalText::sanitize(fragment));
        let boundary = promotable_boundary(&candidate);
        let promoted = &candidate[..boundary];
        let pending = &candidate[boundary..];
        let p = if self.started {
            " ".repeat(TerminalText::width(&prefix))
        } else {
            prefix.clone()
        };
        let rendered = if promoted.is_empty() {
            Vec::new()
        } else {
            MarkdownRenderer::render(&p, promoted, self.width, color)
        };
        self.id = id;
        self.prefix = prefix.clone();
        self.pending = pending.to_owned();
        self.started |= !rendered.is_empty();
        let preview_prefix = if self.started || !rendered.is_empty() {
            " ".repeat(TerminalText::width(&prefix))
        } else {
            prefix
        };
        Ok(MarkdownLiveUpdate {
            scrollback: if rendered.is_empty() {
                None
            } else {
                Some(rendered)
            },
            preview_prefix,
            preview: if pending.is_empty() {
                vec![]
            } else {
                vec![pending.to_owned()]
            },
        })
    }
    pub fn commit(&mut self, color: bool) -> Result<MarkdownLiveUpdate, FinalFormatError> {
        let p = if self.started {
            " ".repeat(TerminalText::width(&self.prefix))
        } else {
            self.prefix.clone()
        };
        let rendered = if self.pending.is_empty() {
            Vec::new()
        } else {
            EnhancedFinalMessageRenderer::render(
                &self.pending,
                &p,
                self.width,
                color,
                !self.started,
            )?
        };
        *self = Self::new(self.width);
        Ok(MarkdownLiveUpdate {
            scrollback: if rendered.is_empty() {
                None
            } else {
                Some(rendered)
            },
            preview_prefix: String::new(),
            preview: vec![],
        })
    }
}
fn promotable_boundary(s: &str) -> usize {
    use termimad::minimad::{CompositeStyle, Line};

    let mut safe = 0;
    let mut pos = 0;
    let mut open_fence = false;
    let mut table = false;
    let mut ordered_list = false;
    while let Some(rel) = s[pos..].find('\n') {
        let nl = pos + rel;
        let line = &s[pos..nl];
        if open_fence {
            if line.starts_with("```") {
                open_fence = false;
                safe = nl + 1;
            }
            pos = nl + 1;
            continue;
        }
        let parsed = Line::from(line);
        let ordered_item = matches!(
            &parsed,
            Line::Normal(composite)
                if matches!(composite.style, CompositeStyle::OrderedListItem { .. })
        );
        let table_part = matches!(&parsed, Line::TableRow(_) | Line::TableRule(_));
        if (ordered_list && !ordered_item && !line.is_empty() && !line.starts_with(' '))
            || (table && !table_part)
        {
            return pos;
        }
        if matches!(parsed, Line::CodeFence(_)) {
            open_fence = true;
        } else if table_part {
            table = true;
        } else if ordered_item || ordered_list {
            ordered_list = true;
        } else {
            safe = nl + 1;
        }
        pos = nl + 1;
    }
    safe
}
