//! Terminal text metrics, wrapping and Markdown rendering for the inline CLI.
//!
//! Cell widths intentionally follow the source implementation's small, stable
//! width table rather than locale-dependent terminal measurements.

mod final_format;
mod terminal_line;

pub use final_format::{EnhancedFinalMessageRenderer, FinalFormatError, ToolYamlFormatter};
pub use terminal_line::{TerminalCellStyleSpan, TerminalLine, TerminalStyle};

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Style {
    code: &'static str,
}

impl Default for Style {
    fn default() -> Self {
        Self { code: "38;5;195" }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Run {
    text: String,
    style: Style,
}

fn add(runs: &mut Vec<Run>, text: impl AsRef<str>, style: Style) {
    let text = text.as_ref();
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut().filter(|last| last.style == style) {
        last.text.push_str(text);
    } else {
        runs.push(Run {
            text: text.to_owned(),
            style,
        });
    }
}

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

/// Markdown rendering that keeps terminal styling as presentation-only ANSI.
pub struct MarkdownRenderer;
impl MarkdownRenderer {
    pub fn render(prefix: &str, source: &str, columns: usize, color: bool) -> Vec<String> {
        let prefix = TerminalText::sanitize(prefix);
        let clean = TerminalText::sanitize(source);
        let source = clean.trim_matches(['\r', '\n']);
        let source = source.trim_start_matches(['\r', '\n']);
        if source.trim().is_empty() {
            return Vec::new();
        }
        let lines: Vec<&str> = source.split('\n').collect();
        let continuation = " ".repeat(TerminalText::width(&prefix));
        let mut out = Vec::new();
        let mut first = true;
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim_end_matches('\r');
            if let Some((marker, minimum, language)) = fence(line) {
                i += 1;
                let mut code = Vec::new();
                while i < lines.len()
                    && !fence_close(lines[i].trim_end_matches('\r'), marker, minimum)
                {
                    code.push(lines[i].trim_end_matches('\r'));
                    i += 1;
                }
                if code.is_empty() {
                    code.push("");
                }
                let highlight_code = color
                    && known_language(&language)
                    && code.len() <= 10_000
                    && code
                        .iter()
                        .map(|line| line.encode_utf16().count() + 1)
                        .sum::<usize>()
                        <= 512 * 1024;
                let mut in_block_comment = false;
                let mut in_triple_string = false;
                for (n, code_line) in code.iter().enumerate() {
                    let p = if first && n == 0 {
                        &prefix
                    } else {
                        &continuation
                    };
                    let runs = if highlight_code {
                        highlight(code_line, &mut in_block_comment, &mut in_triple_string)
                    } else {
                        vec![Run {
                            text: (*code_line).to_owned(),
                            style: Style::default(),
                        }]
                    };
                    out.extend(wrap_runs(
                        prefix_runs(p, runs),
                        &continuation,
                        columns,
                        color,
                    ));
                }
                first = false;
                if i < lines.len() {
                    i += 1;
                }
                continue;
            }
            if let Some((rows, consumed)) = table(&lines, i) {
                let p = if first { &prefix } else { &continuation };
                out.extend(render_table(rows, p, &continuation, columns, color));
                first = false;
                i += consumed;
                continue;
            }
            let p = if first { &prefix } else { &continuation };
            let runs = line_runs(line, columns.saturating_sub(TerminalText::width(p)));
            out.extend(wrap_runs(
                prefix_runs(p, runs),
                &continuation,
                columns,
                color,
            ));
            first = false;
            i += 1;
        }
        if out.is_empty() && !prefix.is_empty() {
            out.push(prefix);
        }
        out
    }
}

fn prefix_runs(prefix: &str, runs: Vec<Run>) -> Vec<Run> {
    if prefix.is_empty() {
        runs
    } else {
        let mut r = vec![Run {
            text: prefix.to_owned(),
            style: Style::default(),
        }];
        for x in runs {
            add(&mut r, x.text, x.style)
        }
        r
    }
}
fn line_runs(line: &str, available: usize) -> Vec<Run> {
    let lead = line.len() - line.trim_start_matches(' ').len();
    let (indent, content) = line.split_at(lead);
    let mut r = Vec::new();
    if let Some(h) = heading(content) {
        add(&mut r, indent, Style::default());
        r.extend(inline_with_basis(h, Style { code: "1;36" }));
        return r;
    }
    if let Some(q) = content.strip_prefix('>') {
        add(&mut r, format!("{indent}│ "), Style { code: "2;90" });
        for x in inline(q.trim_start_matches(' ')) {
            add(&mut r, x.text, x.style)
        }
        return r;
    }
    if let Some((marker, rest)) = list_marker(content) {
        add(&mut r, indent, Style::default());
        add(&mut r, marker, Style { code: "1;36" });
        for x in inline(rest) {
            add(&mut r, x.text, x.style)
        }
        return r;
    }
    if thematic(content) {
        add(&mut r, "─".repeat(available.max(3)), Style { code: "2;90" });
        return r;
    }
    add(&mut r, indent, Style::default());
    for x in inline(content) {
        add(&mut r, x.text, x.style)
    }
    r
}
fn heading(s: &str) -> Option<&str> {
    let n = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && s.as_bytes().get(n) == Some(&b' ') {
        Some(s[n + 1..].trim())
    } else {
        None
    }
}
fn list_marker(s: &str) -> Option<(String, &str)> {
    let b = s.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        let mut m = "• ".to_owned();
        let mut rest = &s[2..];
        let rb = rest.as_bytes();
        if rb.len() >= 4
            && rb[0] == b'['
            && rb[2] == b']'
            && rb[3] == b' '
            && matches!(rb[1], b' ' | b'x' | b'X')
        {
            m.push_str(if rb[1] == b' ' { "☐ " } else { "☑ " });
            rest = &rest[4..];
        }
        return Some((m, rest));
    }
    let d: usize = s
        .chars()
        .take_while(|c| is_code_digit(*c))
        .map(char::len_utf8)
        .sum();
    if d > 0 && b.len() > d + 1 && matches!(b[d], b'.' | b')') && b[d + 1] == b' ' {
        Some((s[..d + 2].to_owned(), &s[d + 2..]))
    } else {
        None
    }
}
fn thematic(s: &str) -> bool {
    let v: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    v.len() >= 3
        && v.chars().all(|c| c == v.chars().next().unwrap())
        && matches!(v.chars().next().unwrap(), '-' | '*' | '_')
}
fn inline(s: &str) -> Vec<Run> {
    inline_with_basis(s, Style::default())
}

fn inline_with_basis(s: &str, basis: Style) -> Vec<Run> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let tail = &s[i..];
        if tail.starts_with('\\') && tail.len() > 1 {
            let c = tail[1..].chars().next().unwrap();
            if is_escapable(c) {
                add(&mut out, c.to_string(), basis);
                i += 1 + c.len_utf8();
                continue;
            }
        }
        let mut matched = false;
        for (mark, style) in [
            ("**", "1;38;5;195"),
            ("__", "1;38;5;195"),
            ("~~", "9;38;5;195"),
            ("*", "3;38;5;195"),
            ("_", "3;38;5;195"),
        ] {
            if let Some(rest) = tail.strip_prefix(mark) {
                if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                    if let Some(end) = rest.find(mark) {
                        let before = &rest[..end];
                        if !before.is_empty() && !before.ends_with(char::is_whitespace) {
                            add(&mut out, before, inline_style(basis, style));
                            i += mark.len() + end + mark.len();
                            matched = true;
                            break;
                        }
                    }
                }
            }
        }
        if matched {
            continue;
        }
        if tail.starts_with('`') {
            let n = tail.chars().take_while(|c| *c == '`').count();
            let marker = "`".repeat(n);
            if let Some(end) = tail[n..].find(&marker) {
                let mut text = &tail[n..n + end];
                if text.len() > 1
                    && text.starts_with(' ')
                    && text.ends_with(' ')
                    && !text.trim().is_empty()
                {
                    text = &text[1..text.len() - 1];
                }
                add(&mut out, text, inline_style(basis, "33"));
                i += n + end + n;
                continue;
            }
        }
        if tail.starts_with('[') {
            if let Some(mid) = tail.find("](") {
                if mid > 1 {
                    if let Some(end) = tail[mid + 2..].find(')') {
                        let label = &tail[1..mid];
                        let url = &tail[mid + 2..mid + 2 + end];
                        add(&mut out, label, inline_style(basis, "4;38;5;195"));
                        add(&mut out, format!(" ({url})"), inline_style(basis, "4;36"));
                        i += mid + 3 + end;
                        continue;
                    }
                }
            }
        }
        let c = tail.chars().next().unwrap();
        add(&mut out, c.to_string(), basis);
        i += c.len_utf8();
    }
    out
}

// Inline bases are assistant text or the source's bold/cyan heading/header style.
fn inline_style(basis: Style, overlay: &'static str) -> Style {
    if basis.code != "1;36" {
        return Style { code: overlay };
    }
    Style {
        code: match overlay {
            "1;38;5;195" => "1;36",
            "3;38;5;195" => "1;3;36",
            "9;38;5;195" => "1;9;36",
            "33" => "1;33",
            "4;38;5;195" | "4;36" => "1;4;36",
            _ => overlay,
        },
    }
}

fn is_escapable(c: char) -> bool {
    // The source predicate takes a UTF-16 char, not a supplementary rune.
    c.len_utf16() == 1
        && matches!(
            get_general_category(c),
            GeneralCategory::ConnectorPunctuation
                | GeneralCategory::DashPunctuation
                | GeneralCategory::OpenPunctuation
                | GeneralCategory::ClosePunctuation
                | GeneralCategory::InitialPunctuation
                | GeneralCategory::FinalPunctuation
                | GeneralCategory::OtherPunctuation
                | GeneralCategory::MathSymbol
                | GeneralCategory::CurrencySymbol
                | GeneralCategory::ModifierSymbol
                | GeneralCategory::OtherSymbol
        )
}

fn wrap_runs(source: Vec<Run>, continuation: &str, columns: usize, color: bool) -> Vec<String> {
    let columns = columns.max(1);
    let cw = TerminalText::width(continuation);
    let avail = columns.saturating_sub(cw).max(1);
    let mut rows: Vec<Vec<Run>> = vec![Vec::new()];
    let mut cells = 0;
    let mut start = 0;
    let mut word: Vec<Run> = Vec::new();
    let mut spaces: Vec<Run> = Vec::new();
    fn measure(rs: &[Run]) -> usize {
        rs.iter().map(|r| TerminalText::width(&r.text)).sum()
    }
    fn emit(out: &mut Vec<Run>, rs: &[Run]) {
        for r in rs {
            add(out, &r.text, r.style)
        }
    }
    fn emit_broken(
        rows: &mut Vec<Vec<Run>>,
        cells: &mut usize,
        start: &mut usize,
        width: usize,
        cont: &str,
        rs: &[Run],
    ) {
        for r in rs {
            for g in graphemes(&r.text) {
                let w = grapheme_width(&g);
                if *cells > *start && *cells + w > width {
                    rows.push(vec![Run {
                        text: cont.to_owned(),
                        style: Style::default(),
                    }]);
                    *cells = TerminalText::width(cont);
                    *start = *cells;
                }
                add(rows.last_mut().unwrap(), g, r.style);
                *cells += w;
            }
        }
    }
    for run in source {
        for g in graphemes(&run.text) {
            if g.chars().next().is_some_and(char::is_whitespace) {
                if !word.is_empty() {
                    let ww = measure(&word);
                    let sw = measure(&spaces);
                    if cells + sw + ww > columns && ww <= avail {
                        rows.push(vec![Run {
                            text: continuation.to_owned(),
                            style: Style::default(),
                        }]);
                        cells = cw;
                        start = cw;
                        emit(rows.last_mut().unwrap(), &word);
                        cells += ww;
                    } else if cells + sw + ww > columns {
                        if cells + sw <= columns {
                            emit(rows.last_mut().unwrap(), &spaces);
                            cells += sw;
                        }
                        emit_broken(
                            &mut rows,
                            &mut cells,
                            &mut start,
                            columns,
                            continuation,
                            &word,
                        );
                    } else {
                        emit(rows.last_mut().unwrap(), &spaces);
                        emit(rows.last_mut().unwrap(), &word);
                        cells += sw + ww;
                    }
                    word.clear();
                    spaces.clear();
                }
                add(&mut spaces, g, run.style);
            } else {
                add(&mut word, g, run.style);
            }
        }
    }
    if !word.is_empty() {
        let ww = measure(&word);
        let sw = measure(&spaces);
        if cells + sw + ww > columns && ww <= avail {
            rows.push(vec![Run {
                text: continuation.to_owned(),
                style: Style::default(),
            }]);
            emit(rows.last_mut().unwrap(), &word);
        } else if cells + sw + ww > columns {
            if cells + sw <= columns {
                emit(rows.last_mut().unwrap(), &spaces);
                cells += sw;
            }
            emit_broken(
                &mut rows,
                &mut cells,
                &mut start,
                columns,
                continuation,
                &word,
            );
        } else {
            emit(rows.last_mut().unwrap(), &spaces);
            emit(rows.last_mut().unwrap(), &word);
        }
        word.clear();
        spaces.clear();
    }
    for run in spaces {
        for g in graphemes(&run.text) {
            let w = grapheme_width(&g);
            if cells + w > columns {
                break;
            }
            add(rows.last_mut().unwrap(), g, run.style);
            cells += w;
        }
    }
    rows.into_iter()
        .map(|rs| {
            rs.into_iter()
                .map(|r| paint(r.text, r.style, color))
                .collect()
        })
        .collect()
}
fn fence(s: &str) -> Option<(char, usize, String)> {
    let t = s.trim_start_matches(' ');
    if s.len() - t.len() > 3 {
        return None;
    }
    let marker = t.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let n = t.chars().take_while(|c| *c == marker).count();
    if n < 3 {
        return None;
    }
    let lang = t[n..]
        .trim()
        .split(' ')
        .find(|part| !part.is_empty())
        .unwrap_or("")
        .trim_matches(['{', '}', '.'])
        .to_ascii_lowercase();
    Some((marker, n, lang))
}
fn fence_close(s: &str, m: char, n: usize) -> bool {
    let t = s.trim_start_matches(' ');
    if s.len() - t.len() > 3 {
        return false;
    }
    let k = t.chars().take_while(|c| *c == m).count();
    k >= n && t[k..].trim().is_empty()
}
fn known_language(s: &str) -> bool {
    let s = match s {
        "c-sharp" | "c#" => "csharp",
        "golang" => "go",
        "python3" | "py3" => "python",
        "shell" | "sh" | "zsh" => "bash",
        _ => s,
    };
    matches!(
        s,
        "bash"
            | "c"
            | "cpp"
            | "csharp"
            | "css"
            | "diff"
            | "go"
            | "html"
            | "java"
            | "javascript"
            | "js"
            | "json"
            | "kotlin"
            | "php"
            | "python"
            | "ruby"
            | "rust"
            | "sql"
            | "swift"
            | "typescript"
            | "ts"
            | "xml"
            | "yaml"
            | "yml"
    )
}
fn highlight(s: &str, in_block_comment: &mut bool, in_triple_string: &mut bool) -> Vec<Run> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let tail = &s[i..];
        if *in_block_comment || *in_triple_string {
            let (marker, style) = if *in_block_comment {
                ("*/", Style { code: "2;3;90" })
            } else {
                ("\"\"\"", Style { code: "32" })
            };
            let end = tail.find(marker);
            let length = end.map_or(tail.len(), |end| end + marker.len());
            add(&mut out, &tail[..length], style);
            i += length;
            if *in_block_comment {
                *in_block_comment = end.is_none();
            } else {
                *in_triple_string = end.is_none();
            }
            continue;
        }
        if tail.starts_with("//") || tail.starts_with('#') {
            add(&mut out, tail, Style { code: "2;3;90" });
            break;
        }
        if tail.starts_with("/*") {
            *in_block_comment = true;
            continue;
        }
        if tail.starts_with("\"\"\"") {
            *in_triple_string = true;
            add(&mut out, "\"\"\"", Style { code: "32" });
            i += 3;
            continue;
        }
        let ch = tail.chars().next().expect("nonempty tail");
        if matches!(ch, '"' | '\'' | '`') {
            let mut end = ch.len_utf8();
            let mut escaped = false;
            for next in tail[end..].chars() {
                end += next.len_utf8();
                if next == ch && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
                if next != '\\' {
                    escaped = false;
                }
            }
            add(&mut out, &tail[..end], Style { code: "32" });
            i += end;
            continue;
        }
        if is_code_digit(ch) {
            let end = tail
                .char_indices()
                .take_while(|(_, c)| is_code_letter_or_digit(*c) || matches!(c, '.' | '_'))
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap();
            add(&mut out, &tail[..end], Style { code: "33" });
            i += end;
            continue;
        }
        if is_code_letter(ch) || ch == '_' {
            let end = tail
                .char_indices()
                .take_while(|(_, c)| is_code_letter_or_digit(*c) || *c == '_')
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(ch.len_utf8());
            let word = &tail[..end];
            let style = if matches!(
                word,
                "abstract"
                    | "and"
                    | "as"
                    | "async"
                    | "await"
                    | "base"
                    | "bool"
                    | "break"
                    | "case"
                    | "catch"
                    | "char"
                    | "class"
                    | "const"
                    | "continue"
                    | "def"
                    | "defer"
                    | "do"
                    | "else"
                    | "enum"
                    | "except"
                    | "export"
                    | "extends"
                    | "false"
                    | "finally"
                    | "float"
                    | "for"
                    | "foreach"
                    | "from"
                    | "func"
                    | "function"
                    | "go"
                    | "if"
                    | "implements"
                    | "import"
                    | "in"
                    | "int"
                    | "interface"
                    | "internal"
                    | "is"
                    | "let"
                    | "map"
                    | "match"
                    | "namespace"
                    | "new"
                    | "nil"
                    | "not"
                    | "null"
                    | "object"
                    | "of"
                    | "or"
                    | "out"
                    | "override"
                    | "package"
                    | "pass"
                    | "private"
                    | "protected"
                    | "public"
                    | "readonly"
                    | "record"
                    | "return"
                    | "sealed"
                    | "static"
                    | "string"
                    | "struct"
                    | "super"
                    | "switch"
                    | "this"
                    | "throw"
                    | "throws"
                    | "true"
                    | "try"
                    | "type"
                    | "typeof"
                    | "undefined"
                    | "using"
                    | "var"
                    | "virtual"
                    | "void"
                    | "when"
                    | "where"
                    | "while"
                    | "with"
                    | "yield"
            ) {
                Style { code: "1;35" }
            } else if tail[end..].trim_start().starts_with('(') {
                Style { code: "36" }
            } else {
                Style::default()
            };
            add(&mut out, word, style);
            i += end;
            continue;
        }
        let size = ch.len_utf8();
        add(
            &mut out,
            &tail[..size],
            if "+-*/%=!<>&|^~?:".contains(ch) {
                Style { code: "36" }
            } else {
                Style::default()
            },
        );
        i += size;
    }
    out
}

fn is_code_letter(c: char) -> bool {
    c.len_utf16() == 1
        && matches!(
            get_general_category(c),
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::ModifierLetter
                | GeneralCategory::OtherLetter
        )
}
fn is_code_digit(c: char) -> bool {
    c.len_utf16() == 1 && get_general_category(c) == GeneralCategory::DecimalNumber
}
fn is_code_letter_or_digit(c: char) -> bool {
    is_code_letter(c) || is_code_digit(c)
}

fn table(lines: &[&str], i: usize) -> Option<(Vec<Vec<String>>, usize)> {
    if i + 1 >= lines.len() {
        return None;
    }
    let header = parse_row(lines[i])?;
    let delim = parse_row(lines[i + 1])?;
    if header.len() != delim.len()
        || !delim.iter().all(|c| {
            let t = c.trim_matches(':');
            t.len() >= 3 && t.chars().all(|x| x == '-')
        })
    {
        return None;
    }
    let mut rows = vec![header];
    let mut j = i + 2;
    while j < lines.len() {
        if let Some(mut r) = parse_row(lines[j]) {
            r.resize(rows[0].len(), String::new());
            r.truncate(rows[0].len());
            rows.push(r);
            j += 1;
        } else {
            break;
        }
    }
    Some((rows, j - i))
}
fn parse_row(s: &str) -> Option<Vec<String>> {
    let t = s.trim();
    if !t.contains('|') {
        return None;
    }
    let t = t.trim_matches('|');
    let v: Vec<String> = t.split('|').map(|c| c.trim().to_owned()).collect();
    if v.len() > 1 { Some(v) } else { None }
}
fn render_table(
    rows: Vec<Vec<String>>,
    prefix: &str,
    cont: &str,
    columns: usize,
    color: bool,
) -> Vec<String> {
    let mut widths = vec![0; rows[0].len()];
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(TerminalText::width(c));
        }
    }
    let required: usize =
        widths.iter().sum::<usize>() + 3 * widths.len() + 1 + TerminalText::width(prefix);
    let mut out = Vec::new();
    if required > columns {
        for row in rows.iter().skip(1) {
            for (i, c) in row.iter().enumerate() {
                let p = if out.is_empty() { prefix } else { cont };
                let mut runs =
                    inline_with_basis(&format!("{}: ", rows[0][i]), Style { code: "1;36" });
                runs.extend(inline(c));
                out.extend(wrap_runs(runs_with_prefix(p, runs), cont, columns, color));
            }
        }
        return out;
    }
    let border = |left: &str, mid: &str, right: &str| {
        let mut s = String::new();
        s.push_str(left);
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(mid);
            }
            s.push_str(&"─".repeat(w + 2));
        }
        s.push_str(right);
        s
    };
    out.push(paint(
        format!("{prefix}{}", border("┌", "┬", "┐")),
        Style { code: "2;90" },
        color,
    ));
    for (ri, row) in rows.iter().enumerate() {
        let mut runs = vec![Run {
            text: format!("{cont}│"),
            style: Style { code: "2;90" },
        }];
        for (i, c) in row.iter().enumerate() {
            let basis = if ri == 0 {
                Style { code: "1;36" }
            } else {
                Style::default()
            };
            runs.extend(inline_with_basis(&format!(" {c}"), basis));
            add(
                &mut runs,
                " ".repeat(widths[i].saturating_sub(TerminalText::width(c)) + 1),
                Style::default(),
            );
            add(&mut runs, "│", Style { code: "2;90" });
        }
        out.extend(wrap_runs(runs, cont, required, color));
        if ri == 0 {
            out.push(paint(
                format!("{cont}{}", border("├", "┼", "┤")),
                Style { code: "2;90" },
                color,
            ));
        }
    }
    out.push(paint(
        format!("{cont}{}", border("└", "┴", "┘")),
        Style { code: "2;90" },
        color,
    ));
    out
}
fn runs_with_prefix(p: &str, r: Vec<Run>) -> Vec<Run> {
    let mut v = vec![Run {
        text: p.to_owned(),
        style: Style::default(),
    }];
    for x in r {
        add(&mut v, x.text, x.style)
    }
    v
}
fn paint(s: String, style: Style, color: bool) -> String {
    if color && !s.is_empty() && !style.code.is_empty() {
        format!("\x1b[{}m{}\x1b[0m", style.code, s)
    } else {
        s
    }
}

/// Small live-stream accumulator: only newline-complete safe regions are
/// promoted; incomplete fences/tables remain preview text until a boundary or commit.
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
    let mut safe = 0;
    let mut pos = 0;
    let mut open_fence: Option<(char, usize)> = None;
    let mut table = false;
    while let Some(rel) = s[pos..].find('\n') {
        let nl = pos + rel;
        let line = s[pos..nl].trim_end_matches('\r');
        if let Some((m, n)) = open_fence {
            if fence_close(line, m, n) {
                open_fence = None;
                safe = nl + 1;
            }
            pos = nl + 1;
            continue;
        }
        if let Some((m, n, _)) = fence(line) {
            open_fence = Some((m, n));
            pos = nl + 1;
            continue;
        }
        if line.contains('|') {
            table = true;
            pos = nl + 1;
            continue;
        }
        if table {
            safe = pos;
            return safe;
        }
        safe = nl + 1;
        pos = nl + 1;
    }
    safe
}
