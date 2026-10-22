use std::{fmt::Write, sync::LazyLock};

use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style, ThemeSet},
    parsing::SyntaxSet,
};
use termimad::{
    FmtText, MadSkin,
    minimad::{Composite, Line, Options, Text, parse_text},
};
use unicode_segmentation::UnicodeSegmentation;

use super::TerminalText;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Markdown formatting without terminal I/O. The caller owns drawing and flushing.
pub struct MarkdownRenderer;

impl MarkdownRenderer {
    pub fn render(prefix: &str, source: &str, columns: usize, color: bool) -> Vec<String> {
        let prefix = TerminalText::sanitize_line(prefix);
        let source = TerminalText::sanitize(source);
        let source = source.trim_matches('\n');
        if source.trim().is_empty() {
            return Vec::new();
        }
        let width = columns
            .max(1)
            .saturating_sub(TerminalText::width(&prefix))
            .max(1);
        let skin = MadSkin::default();
        let parsed = parse_text(source, Options::default().keep_code_fences(true));
        let mut lines = parsed.lines.into_iter();
        let mut ordinary = Vec::new();
        let mut output = Vec::new();
        while let Some(line) = lines.next() {
            if !matches!(line, Line::CodeFence(_)) {
                ordinary.push(line);
                continue;
            }
            render_ordinary(&skin, &mut ordinary, width, color, &mut output);
            let language = line.code_fence_lang().unwrap_or_default();
            let mut code = Vec::new();
            for line in lines.by_ref() {
                match line {
                    Line::CodeFence(_) => break,
                    Line::Normal(composite) => code.push(
                        composite
                            .compounds
                            .iter()
                            .map(|compound| compound.src)
                            .collect::<String>(),
                    ),
                    _ => unreachable!("minimad emits only code between fences"),
                }
            }
            render_code(&code, language, width, color, &mut output);
        }
        render_ordinary(&skin, &mut ordinary, width, color, &mut output);
        let continuation = " ".repeat(TerminalText::width(&prefix));
        output
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                format!("{}{line}", if index == 0 { &prefix } else { &continuation })
            })
            .collect()
    }
}

fn render_ordinary(
    skin: &MadSkin,
    lines: &mut Vec<Line<'_>>,
    width: usize,
    color: bool,
    output: &mut Vec<String>,
) {
    if lines.is_empty() {
        return;
    }
    let text = Text {
        lines: std::mem::take(lines),
    };
    let needs_fallback = width < 3
        || text.lines.iter().any(|line| match line {
            Line::Normal(composite) => contains_cluster(composite),
            Line::TableRow(row) => {
                row.cells.len() * 4 + 1 > width || row.cells.iter().any(contains_cluster)
            }
            _ => false,
        });
    // Termimad wraps scalars and may drop table columns when space is insufficient.
    // In those cases format intact content, then wrap with the CLI's grapheme metrics.
    let mut formatted = FmtText::from_text(skin, text, (!needs_fallback).then_some(width));
    formatted.set_rendering_width(width);
    let formatted = formatted.to_string();
    let formatted = formatted.strip_suffix('\n').unwrap_or(&formatted);
    for line in formatted.split('\n') {
        output.extend(wrap_spans(&trusted_sgr_spans(line), width, color));
    }
}

fn contains_cluster(composite: &Composite<'_>) -> bool {
    composite
        .compounds
        .iter()
        .map(|compound| compound.src)
        .collect::<String>()
        .graphemes(true)
        .any(|grapheme| grapheme.chars().nth(1).is_some())
}

fn render_code(
    code: &[String],
    language: &str,
    width: usize,
    color: bool,
    output: &mut Vec<String>,
) {
    let token = language.split_whitespace().next().unwrap_or_default();
    let token = token.to_ascii_lowercase();
    let token = match token.as_str() {
        "csharp" | "c-sharp" | "c#" => "cs",
        "golang" => "go",
        "python3" | "py3" => "py",
        "shell" | "sh" | "zsh" => "bash",
        token => token,
    };
    let syntax = if color
        && code.len() <= 10_000
        && code
            .iter()
            .map(|line| line.encode_utf16().count() + 1)
            .sum::<usize>()
            <= 512 * 1024
    {
        SYNTAXES.find_syntax_by_token(token)
    } else {
        None
    };
    let mut highlighter =
        syntax.map(|syntax| HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"]));
    if code.is_empty() {
        output.push(String::new());
    }
    for line in code {
        let spans = highlighter
            .as_mut()
            .and_then(|highlighter| highlighter.highlight_line(line, &SYNTAXES).ok());
        let spans = if let Some(spans) = spans {
            spans
                .into_iter()
                .map(|(style, text)| (syntax_style(style), text))
                .collect()
        } else {
            // A failed parser state must not affect subsequent lines of the block.
            highlighter = None;
            vec![(String::new(), line.as_str())]
        };
        output.extend(wrap_spans(&spans, width, color));
    }
}

fn syntax_style(style: Style) -> String {
    let mut sgr = format!(
        "\x1b[38;2;{};{};{}m",
        style.foreground.r, style.foreground.g, style.foreground.b
    );
    for (flag, code) in [
        (FontStyle::BOLD, 1),
        (FontStyle::ITALIC, 3),
        (FontStyle::UNDERLINE, 4),
    ] {
        if style.font_style.contains(flag) {
            write!(sgr, "\x1b[{code}m").expect("writing to a String cannot fail");
        }
    }
    sgr
}

// Only Termimad-generated SGR is accepted here, never unsanitized terminal input.
fn trusted_sgr_spans(line: &str) -> Vec<(String, &str)> {
    let mut spans = Vec::new();
    let mut remaining = line;
    let mut style = String::new();
    while let Some(escape) = remaining.find('\x1b') {
        if escape > 0 {
            spans.push((style.clone(), &remaining[..escape]));
        }
        remaining = &remaining[escape..];
        let end = remaining.find('m').expect("Termimad emits SGR styling");
        let command = &remaining[..=end];
        assert!(command.starts_with("\x1b["));
        assert!(
            command[2..end]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b';')
        );
        if command == "\x1b[0m" || command == "\x1b[m" {
            style.clear();
        } else {
            style.push_str(command);
        }
        remaining = &remaining[end + 1..];
    }
    if !remaining.is_empty() {
        spans.push((style, remaining));
    }
    spans
}

fn wrap_spans(spans: &[(String, &str)], width: usize, color: bool) -> Vec<String> {
    let text: String = spans.iter().map(|(_, text)| *text).collect();
    let mut ranges = Vec::with_capacity(spans.len());
    let mut end = 0;
    for (style, text) in spans {
        end += text.len();
        ranges.push((end, style.as_str()));
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    let mut active = "";
    let mut span = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        while ranges[span].0 <= offset {
            span += 1;
        }
        let style = if color { ranges[span].1 } else { "" };
        let size = TerminalText::width(grapheme);
        if cells > 0 && cells + size > width {
            if !active.is_empty() {
                row.push_str("\x1b[0m");
            }
            rows.push(std::mem::take(&mut row));
            cells = 0;
            active = "";
        }
        if style != active {
            if !active.is_empty() {
                row.push_str("\x1b[0m");
            }
            row.push_str(style);
            active = style;
        }
        row.push_str(grapheme);
        cells += size;
    }
    if !active.is_empty() {
        row.push_str("\x1b[0m");
    }
    rows.push(row);
    rows
}
