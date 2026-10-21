//! Source tool-report rendering over the delivered T71 frame interfaces.

use std::{io, sync::Arc};

use crate::{
    terminal_frames::{
        ItemIdentity, LiveBufferItem, LiveBufferRender, LiveBufferRenderContext,
        LiveBufferRetention, ScrollbackItem, ScrollbackLayout, ScrollbackRenderContext,
    },
    terminal_layout::{MarkdownRenderer, TerminalLine, TerminalStyle, TerminalText},
};

use super::model::{
    Presentation, ToolBlock, ToolBlockKind, ToolPresentationMetadata, ToolPresentationStyle,
    ToolReport, ToolTerminalStatus,
};

const MAXIMUM_DETAIL_BYTES: usize = 16 * 1024;
const MAXIMUM_LABEL_BYTES: usize = 1024;
const MAXIMUM_DIFF_ROWS: usize = 100;

/// Prefix shared by root and hierarchical activity reports. The default still
/// reserves the source's two-cell marker column.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActivityDecoration {
    pub indentation: String,
    pub label_field: String,
    pub adornment_field: String,
}
impl ActivityDecoration {
    pub fn describe(columns: usize, depth: usize, label: Option<&str>, adornment: &str) -> Self {
        let available = columns.saturating_sub(1);
        let indentation = " ".repeat(depth.saturating_mul(2).min(available.saturating_sub(2)));
        let mut remaining = available.saturating_sub(TerminalText::width(&indentation) + 2);
        let clean_label = TerminalText::sanitize(label.unwrap_or_default()).replace('\n', "");
        let label_field = if clean_label.is_empty() {
            String::new()
        } else {
            TerminalText::clip(&format!("[{clean_label}] "), remaining)
        };
        remaining = remaining.saturating_sub(TerminalText::width(&label_field));
        let adornment = TerminalText::sanitize(adornment).replace('\n', "");
        let adornment_field = if adornment.is_empty() {
            String::new()
        } else {
            TerminalText::clip(&format!("{adornment} "), remaining)
        };
        Self {
            indentation,
            label_field,
            adornment_field,
        }
    }
    pub fn width(&self) -> usize {
        TerminalText::width(&self.indentation)
            + 2
            + TerminalText::width(&self.label_field)
            + TerminalText::width(&self.adornment_field)
    }
    pub fn content_columns(&self, columns: usize) -> usize {
        columns.saturating_sub(self.width()).max(1)
    }
    pub fn apply(&self, marker: &str, content: impl IntoIterator<Item = String>) -> Vec<String> {
        let marker = if marker.is_empty() {
            "  ".to_owned()
        } else {
            format!("{marker} ")
        };
        let lead = format!(
            "{}{marker}{}{}",
            self.indentation, self.label_field, self.adornment_field
        );
        let continuation = format!(
            "{}  {}{}",
            self.indentation,
            self.label_field,
            " ".repeat(TerminalText::width(&self.adornment_field))
        );
        content
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                format!("{}{value}", if index == 0 { &lead } else { &continuation })
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }
}

pub struct ToolDisplayText;
impl ToolDisplayText {
    pub fn label(value: &str) -> String {
        display_label(value)
    }
    pub fn truncate_utf8(value: &str, maximum_bytes: usize) -> (String, bool) {
        let mut end = maximum_bytes.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        (value[..end].to_owned(), end < value.len())
    }
    pub fn details(values: &[String], maximum_lines: usize) -> Vec<String> {
        assert!(maximum_lines > 0, "maximum_lines must be positive");
        let mut lines = Vec::new();
        let mut line = String::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut truncated_lines = 0;
        'values: for (index, value) in values.iter().enumerate() {
            for (offset, rune) in value.char_indices() {
                if lines.len() == maximum_lines {
                    truncated = true;
                    truncated_lines = count_lines(&value[offset..])
                        + values[index + 1..]
                            .iter()
                            .map(|s| count_lines(s))
                            .sum::<usize>();
                    break 'values;
                }
                if rune == '\n' {
                    lines.push(std::mem::take(&mut line));
                    continue;
                }
                let needed = if rune == '\t' {
                    4
                } else if rune.is_control() {
                    0
                } else {
                    rune.len_utf8()
                };
                if bytes + needed > MAXIMUM_DETAIL_BYTES {
                    truncated = true;
                    truncated_lines = count_lines(&value[offset..])
                        + values[index + 1..]
                            .iter()
                            .map(|s| count_lines(s))
                            .sum::<usize>();
                    break 'values;
                }
                if rune == '\t' {
                    line.push_str("    ");
                } else if !rune.is_control() {
                    line.push(rune);
                }
                bytes += needed;
            }
            if lines.len() == maximum_lines {
                truncated = true;
                truncated_lines = values[index + 1..].iter().map(|s| count_lines(s)).sum();
                break;
            }
            lines.push(std::mem::take(&mut line));
        }
        if truncated {
            if !line.is_empty() && lines.len() < maximum_lines {
                lines.push(line);
            }
            while lines.len() >= maximum_lines {
                bytes -= lines.pop().map_or(0, |value| value.len());
                truncated_lines += 1;
            }
            let truncation = format!(".. {truncated_lines} lines truncated.");
            while bytes + truncation.len() > MAXIMUM_DETAIL_BYTES {
                let Some(last) = lines.last_mut() else {
                    break;
                };
                let other_bytes = bytes - last.len();
                let allowed = MAXIMUM_DETAIL_BYTES.saturating_sub(truncation.len() + other_bytes);
                if allowed > 0 {
                    *last = Self::truncate_utf8(last, allowed).0;
                    break;
                }
                bytes -= last.len();
                lines.pop();
            }
            lines.push(truncation);
        }
        lines
    }
    pub fn layout_details(
        values: &[String],
        columns: usize,
        maximum_lines: usize,
        maximum_detail_lines: usize,
    ) -> Vec<String> {
        let rendered = Self::details(values, maximum_detail_lines)
            .into_iter()
            .flat_map(|value| TerminalText::layout_words(&value, columns))
            .collect::<Vec<_>>();
        if rendered.len() <= maximum_lines {
            return rendered;
        }
        let mut bounded = rendered
            .iter()
            .take(maximum_lines.saturating_sub(1))
            .cloned()
            .collect::<Vec<_>>();
        if maximum_lines > 0 {
            bounded.extend(
                TerminalText::layout_words(
                    &format!(".. {} lines truncated.", rendered.len() - bounded.len()),
                    columns,
                )
                .into_iter()
                .take(1),
            );
        }
        bounded
    }
}
fn count_lines(value: &str) -> usize {
    value.bytes().filter(|&c| c == b'\n').count() + 1
}

pub(crate) fn display_label(value: &str) -> String {
    let first_line = value.split('\n').next().unwrap_or_default();
    let mut units = 0;
    let mut prefix_end = 0;
    for (index, c) in first_line.char_indices() {
        if units + c.len_utf16() > MAXIMUM_LABEL_BYTES {
            break;
        }
        units += c.len_utf16();
        prefix_end = index + c.len_utf8();
    }
    let (bounded, byte_truncated) =
        ToolDisplayText::truncate_utf8(&first_line[..prefix_end], MAXIMUM_LABEL_BYTES);
    let truncated = prefix_end < first_line.len() || byte_truncated;
    let label = TerminalText::sanitize(&bounded);
    if label.len() <= MAXIMUM_LABEL_BYTES && !truncated {
        label
    } else {
        ToolDisplayText::truncate_utf8(&label, MAXIMUM_LABEL_BYTES - '…'.len_utf8()).0 + "…"
    }
}
fn style(color: bool, code: &str) -> TerminalStyle {
    TerminalStyle::new(if color { code } else { "" })
}
fn muted(color: bool) -> TerminalStyle {
    style(color, "\x1b[38;5;245m")
}
fn status_style(
    status: Option<ToolTerminalStatus>,
    metadata: &ToolPresentationMetadata,
    color: bool,
) -> (&str, TerminalStyle) {
    match status {
        Some(ToolTerminalStatus::Succeeded) => (
            if metadata.success_icon.is_empty() {
                "✓"
            } else {
                &metadata.success_icon
            },
            if metadata.style == ToolPresentationStyle::Muted {
                muted(color)
            } else {
                style(color, "\x1b[32m")
            },
        ),
        Some(ToolTerminalStatus::Cancelled) => ("■", muted(color)),
        _ => ("✗", style(color, "\x1b[31m")),
    }
}

impl LiveBufferItem for Presentation {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        self.render_live_decorated(context, &ActivityDecoration::default())
    }
}
impl Presentation {
    pub fn render_live_decorated(
        &self,
        context: LiveBufferRenderContext,
        decoration: &ActivityDecoration,
    ) -> io::Result<LiveBufferRender> {
        let columns = decoration.content_columns(context.columns);
        let (lines, header_count, metadata) = match self {
            Self::Tool(report) => {
                let label = report.timer.as_ref().map_or_else(
                    || report.label.clone(),
                    |timer| format!("{} ({})", report.label, timer()),
                );
                let mut header = TerminalText::layout_words(&label, columns)
                    .into_iter()
                    .take(10)
                    .collect::<Vec<_>>();
                let header_count = header.len();
                let details = if report.block.kind == ToolBlockKind::None {
                    vec![]
                } else {
                    vec![report.block.text.clone()]
                };
                header.extend(ToolDisplayText::layout_details(
                    &details,
                    columns,
                    10usize.saturating_sub(header_count),
                    10,
                ));
                let spinner = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
                    .chars()
                    .nth(report.frame % 10)
                    .unwrap_or('⠋')
                    .to_string();
                (
                    decoration.apply(&spinner, header),
                    header_count,
                    report.metadata.clone(),
                )
            }
            Self::Lines { lines, .. } => (lines.clone(), 0, ToolPresentationMetadata::default()),
            _ => (
                self.render_scrollback_decorated(
                    ScrollbackRenderContext {
                        columns: context.columns,
                        color: context.color,
                        inline_diff: false,
                        previous_packing: None,
                    },
                    decoration,
                )?,
                0,
                ToolPresentationMetadata::default(),
            ),
        };
        let header_style = if metadata.style == ToolPresentationStyle::Muted {
            style(context.color, "\x1b[48;5;236m\x1b[38;5;245m")
        } else {
            style(context.color, "\x1b[48;5;236m\x1b[36m")
        };
        let surface = style(context.color, "\x1b[48;5;236m\x1b[38;5;252m");
        Ok(LiveBufferRender {
            lines: lines
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    TerminalLine::new(
                        line,
                        if index < header_count {
                            header_style.clone()
                        } else {
                            surface.clone()
                        },
                    )
                })
                .collect(),
            caret: None,
            retention: LiveBufferRetention::Tail,
        })
    }
    pub fn render_scrollback_decorated(
        &self,
        context: ScrollbackRenderContext,
        decoration: &ActivityDecoration,
    ) -> io::Result<Vec<String>> {
        match self {
            Self::Tool(report) => render_tool(report, &context, decoration),
            Self::Lines { lines, .. } => Ok(lines.clone()),
            Self::AgentSpawn {
                name,
                metadata,
                prompt,
            } => Ok(render_spawn(name, metadata, prompt, &context, decoration)),
            Self::Diff {
                tool_name,
                path,
                diff,
            } => {
                let previous = context
                    .previous_packing
                    .as_ref()
                    .and_then(|identity| identity.downcast_ref::<DiffPacking>());
                if previous.is_none_or(|previous| previous.tool_name != *tool_name) {
                    return render_tool(
                        &ToolReport::terminal(
                            format!("{tool_name} {path}"),
                            ToolBlock::from_diff(diff),
                            ToolTerminalStatus::Succeeded,
                            ToolPresentationMetadata::default(),
                        ),
                        &context,
                        decoration,
                    );
                }
                let heading = if previous.is_some_and(|previous| previous.path == *path) {
                    ""
                } else {
                    path
                };
                let lines = render_diff(
                    heading,
                    diff,
                    decoration.content_columns(context.columns),
                    context.color,
                    context.inline_diff,
                )?;
                Ok(decoration.apply("", lines))
            }
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffPacking {
    tool_name: String,
    path: String,
}
impl ScrollbackItem for Presentation {
    fn is_completed(&self) -> bool {
        true
    }
    fn layout(&self) -> ScrollbackLayout {
        match self {
            Self::Tool(report) if report.block.kind == ToolBlockKind::None => {
                ScrollbackLayout::Compact
            }
            Self::Lines { layout, .. } => *layout,
            _ => ScrollbackLayout::Block,
        }
    }
    fn ends_layout(&self) -> bool {
        !matches!(self, Self::Diff { .. })
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        match self {
            Self::Diff {
                tool_name, path, ..
            } => Some(Arc::new(DiffPacking {
                tool_name: tool_name.clone(),
                path: path.clone(),
            })),
            _ => None,
        }
    }
    fn packs(&self, previous: Option<&ItemIdentity>) -> bool {
        match (
            self,
            previous.and_then(|previous| previous.downcast_ref::<DiffPacking>()),
        ) {
            (Self::Diff { tool_name, .. }, Some(previous)) => *tool_name == previous.tool_name,
            _ => false,
        }
    }
    fn continues(&self, _previous: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        self.render_scrollback_decorated(context, &ActivityDecoration::default())
    }
}
fn render_tool(
    report: &ToolReport,
    context: &ScrollbackRenderContext,
    decoration: &ActivityDecoration,
) -> io::Result<Vec<String>> {
    let columns = decoration.content_columns(context.columns);
    let (marker, status_style) = status_style(report.status, &report.metadata, context.color);
    let mut header = TerminalText::layout_words(&report.label, columns);
    if report.metadata.multiline_label && header.len() > 5 {
        let omitted = header.len() - 5;
        header.truncate(5);
        header.push(format!(".. {omitted} lines truncated."));
    } else {
        header.truncate(10);
    }
    let header_count = header.len();
    let block = &report.block;
    let block_lines = match block.kind {
        ToolBlockKind::Diff => MAXIMUM_DIFF_ROWS + 2,
        ToolBlockKind::CompletedInput => 100,
        ToolBlockKind::Queue => 30,
        _ => 10,
    };
    let maximum_lines = block_lines.saturating_sub(header_count);
    let detail = if block.kind == ToolBlockKind::Status {
        TerminalText::sanitize(&block.text)
            .split('\n')
            .flat_map(|line| TerminalText::layout_words(line, columns))
            .collect()
    } else if maximum_lines == 0 || block.kind == ToolBlockKind::None {
        vec![]
    } else if block.kind == ToolBlockKind::Diff {
        bound(
            render_diff("", &block.text, columns, context.color, context.inline_diff)?,
            maximum_lines,
            "… diff output truncated",
            columns,
            context.color,
        )
    } else if block.kind == ToolBlockKind::CompletedInput {
        bound(
            MarkdownRenderer::render(
                "",
                &format!("```yaml\n{}\n```", block.text),
                columns,
                context.color,
            ),
            maximum_lines,
            "… completed input truncated",
            columns,
            context.color,
        )
    } else {
        let values = ToolDisplayText::layout_details(
            &[block.text.clone()],
            columns,
            maximum_lines,
            if block.kind == ToolBlockKind::Queue {
                maximum_lines
            } else {
                10
            },
        );
        if matches!(block.kind, ToolBlockKind::Queue | ToolBlockKind::Output) {
            values
        } else {
            values.iter().map(|line| status_style.apply(line)).collect()
        }
    };
    header.extend(detail);
    Ok(decoration
        .apply(marker, header)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index < header_count {
                status_style.apply(&line)
            } else {
                line
            }
        })
        .collect())
}
fn bound(
    mut lines: Vec<String>,
    maximum_lines: usize,
    truncated: &str,
    columns: usize,
    color: bool,
) -> Vec<String> {
    if lines.len() <= maximum_lines {
        return lines;
    }
    lines.truncate(maximum_lines.saturating_sub(1));
    lines.push(muted(color).apply(&TerminalText::clip(truncated, columns)));
    lines
}
fn render_spawn(
    name: &str,
    metadata: &str,
    prompt: &str,
    context: &ScrollbackRenderContext,
    decoration: &ActivityDecoration,
) -> Vec<String> {
    let columns = decoration.content_columns(context.columns);
    let heading = if name.is_empty() {
        "Started agent".to_owned()
    } else {
        format!("Started {}", TerminalText::sanitize_line(name))
    };
    let bold = style(context.color, "\x1b[1m");
    let mut offset = 0;
    let mut lines = Vec::new();
    for line in TerminalText::layout_words(&heading, columns) {
        offset = heading[offset..]
            .find(&line)
            .map_or(offset, |found| offset + found);
        let plain_length = 8usize.saturating_sub(offset).min(line.len());
        lines.push(if name.is_empty() {
            line.clone()
        } else {
            format!(
                "{}{}",
                &line[..plain_length],
                bold.apply(&line[plain_length..])
            )
        });
        offset += line.len();
    }
    if !metadata.is_empty() {
        lines.extend(
            TerminalText::layout_words(&TerminalText::sanitize_line(metadata), columns)
                .iter()
                .map(|line| muted(context.color).apply(line)),
        );
    }
    lines.push(String::new());
    lines.extend(TerminalText::layout_words(
        &TerminalText::sanitize(prompt),
        columns,
    ));
    decoration.apply("♟", lines)
}

#[derive(Debug)]
struct DiffFile {
    old_path: String,
    new_path: String,
    hunks: Vec<DiffHunk>,
}
#[derive(Debug)]
struct DiffHunk {
    old_start: i32,
    old_count: i32,
    new_start: i32,
    new_count: i32,
    rows: Vec<(char, String)>,
}
fn invalid_diff_number() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Diff hunk line number is outside Int32.",
    )
}
fn hunk_numbers(value: &str) -> io::Result<Option<(i32, i32, i32, i32)>> {
    let Some(value) = value.strip_prefix("@@ -") else {
        return Ok(None);
    };
    let Some((old, rest)) = value.split_once(" +") else {
        return Ok(None);
    };
    let Some((new, _)) = rest.split_once(" @@") else {
        return Ok(None);
    };
    fn pair(value: &str) -> io::Result<Option<(i32, i32)>> {
        let (start, count) = value.split_once(',').unwrap_or((value, "1"));
        if start.is_empty()
            || count.is_empty()
            || !start
                .bytes()
                .chain(count.bytes())
                .all(|value| value.is_ascii_digit())
        {
            return Ok(None);
        }
        Ok(Some((
            start.parse().map_err(|_| invalid_diff_number())?,
            count.parse().map_err(|_| invalid_diff_number())?,
        )))
    }
    Ok(match (pair(old)?, pair(new)?) {
        (Some((old_start, old_count)), Some((new_start, new_count))) => {
            Some((old_start, old_count, new_start, new_count))
        }
        _ => None,
    })
}
fn structured_diff(source: &[&str]) -> io::Result<Option<Vec<DiffFile>>> {
    let mut files = Vec::new();
    let mut index = 0;
    while index < source.len() {
        let Some(old_path) = source[index].strip_prefix("--- ") else {
            return Ok(None);
        };
        let Some(new_path) = source
            .get(index + 1)
            .and_then(|line| line.strip_prefix("+++ "))
        else {
            return Ok(None);
        };
        index += 2;
        let mut hunks = Vec::new();
        if source.get(index) == Some(&"Binary files differ") {
            return Ok(None);
        }
        while index < source.len() && !source[index].starts_with("--- ") {
            let Some((old_start, old_count, new_start, new_count)) = hunk_numbers(source[index])?
            else {
                return Ok(None);
            };
            index += 1;
            let mut rows = Vec::new();
            while index < source.len()
                && !source[index].starts_with("@@ ")
                && !source[index].starts_with("--- ")
            {
                let Some(kind @ (' ' | '-' | '+')) = source[index].chars().next() else {
                    return Ok(None);
                };
                rows.push((kind, source[index][1..].to_owned()));
                index += 1;
            }
            hunks.push(DiffHunk {
                old_start,
                old_count,
                new_start,
                new_count,
                rows,
            });
        }
        files.push(DiffFile {
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
            hunks,
        });
    }
    Ok(if files.is_empty() { None } else { Some(files) })
}
fn chunk_report(value: &str) -> bool {
    let Some(value) = value.strip_prefix("Chunk ") else {
        return false;
    };
    let Some((chunk, value)) = value.split_once(" has ") else {
        return false;
    };
    let Some(count) = value
        .strip_suffix(" matches.")
        .or_else(|| value.strip_suffix(" match."))
    else {
        return false;
    };
    !chunk.is_empty()
        && !count.is_empty()
        && chunk
            .bytes()
            .chain(count.bytes())
            .all(|byte| byte.is_ascii_digit())
}
fn expand_tabs(value: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for rune in value.chars() {
        if rune == '\t' {
            let spaces = 4 - column % 4;
            out.push_str(&" ".repeat(spaces));
            column += spaces;
        } else {
            out.push(rune);
            column += TerminalText::rune_width(rune);
        }
    }
    out
}
fn hunk_heading(hunk: &DiffHunk) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    )
}
fn file_heading(file: &DiffFile) -> String {
    let old = file
        .old_path
        .strip_prefix("a/")
        .or_else(|| file.old_path.strip_prefix("b/"))
        .unwrap_or(&file.old_path);
    let new = file
        .new_path
        .strip_prefix("a/")
        .or_else(|| file.new_path.strip_prefix("b/"))
        .unwrap_or(&file.new_path);
    if old == new {
        new.to_owned()
    } else {
        format!("{old} → {new}")
    }
}
fn inline_rows(files: &[DiffFile]) -> Vec<String> {
    let mut output = Vec::new();
    for file in files {
        if files.len() > 1 {
            output.push(file_heading(file));
        }
        for hunk in &file.hunks {
            output.push(hunk_heading(hunk));
            let (mut old_line, mut new_line) = (hunk.old_start, hunk.new_start);
            for (kind, text) in &hunk.rows {
                output.push(match kind {
                    ' ' => format!("{new_line}  {}", expand_tabs(text)),
                    '-' => format!("{old_line} -{}", expand_tabs(text)),
                    _ => format!("{new_line} +{}", expand_tabs(text)),
                });
                if *kind != '+' {
                    old_line = old_line.wrapping_add(1);
                }
                if *kind != '-' {
                    new_line = new_line.wrapping_add(1);
                }
            }
        }
    }
    output
}
fn diff_cell(line: i32, marker: char, text: &str, digits: usize, content_width: usize) -> String {
    let content = TerminalText::clip(&expand_tabs(text), content_width);
    format!(
        "{line:>digits$} {marker}{content}{}",
        " ".repeat(content_width.saturating_sub(content.encode_utf16().count()))
    )
}
fn side_by_side_rows(files: &[DiffFile], columns: usize) -> Vec<String> {
    let maximum_line = files
        .iter()
        .flat_map(|file| &file.hunks)
        .flat_map(|hunk| {
            [
                hunk.old_start.wrapping_add(hunk.rows.len() as i32),
                hunk.new_start.wrapping_add(hunk.rows.len() as i32),
            ]
        })
        .max()
        .unwrap_or(1);
    let digits = maximum_line.to_string().len();
    let cell_width = columns.saturating_sub(TerminalText::width(" │ ")) / 2;
    let Some(content_width) = cell_width
        .checked_sub(digits + 2)
        .filter(|&width| width >= 4)
    else {
        return inline_rows(files);
    };
    let mut output = Vec::new();
    for file in files {
        if files.len() > 1 {
            output.push(file_heading(file));
        }
        for hunk in &file.hunks {
            output.push(hunk_heading(hunk));
            let (mut old_line, mut new_line, mut index) = (hunk.old_start, hunk.new_start, 0);
            while index < hunk.rows.len() {
                let (kind, text) = &hunk.rows[index];
                if *kind == ' ' {
                    output.push(format!(
                        "{} │ {}",
                        diff_cell(old_line, ' ', text, digits, content_width),
                        diff_cell(new_line, ' ', text, digits, content_width)
                    ));
                    old_line = old_line.wrapping_add(1);
                    new_line = new_line.wrapping_add(1);
                    index += 1;
                    continue;
                }
                let (mut removed, mut added) = (Vec::new(), Vec::new());
                while index < hunk.rows.len() && hunk.rows[index].0 != ' ' {
                    let (kind, text) = &hunk.rows[index];
                    index += 1;
                    if *kind == '-' {
                        removed.push((old_line, text));
                        old_line = old_line.wrapping_add(1);
                    } else {
                        added.push((new_line, text));
                        new_line = new_line.wrapping_add(1);
                    }
                }
                for pair in 0..removed.len().max(added.len()) {
                    let left = removed.get(pair).map_or_else(
                        || " ".repeat(cell_width),
                        |(line, text)| diff_cell(*line, '-', text, digits, content_width),
                    );
                    let right = added.get(pair).map_or_else(
                        || " ".repeat(cell_width),
                        |(line, text)| diff_cell(*line, '+', text, digits, content_width),
                    );
                    output.push(format!("{left} │ {right}"));
                }
            }
        }
    }
    output
}
fn is_change(line: &str, marker: char) -> bool {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && line.as_bytes().get(digits) == Some(&b' ')
        && line[digits + 1..].starts_with(marker)
}
fn diff_style(line: &str, color: bool) -> TerminalStyle {
    if is_change(line, '+') || (line.starts_with('+') && !line.starts_with("+++")) {
        style(color, "\x1b[32m")
    } else if is_change(line, '-') || (line.starts_with('-') && !line.starts_with("---")) {
        style(color, "\x1b[31m")
    } else if line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++") {
        muted(color)
    } else {
        TerminalStyle::default()
    }
}
fn render_diff(
    status: &str,
    diff: &str,
    columns: usize,
    color: bool,
    inline: bool,
) -> io::Result<Vec<String>> {
    let mut output = Vec::new();
    let status = TerminalText::sanitize(status);
    let status = status.trim_end_matches(['\r', '\n']);
    if !status.is_empty() {
        output.extend(
            TerminalText::layout_words(status, columns)
                .iter()
                .map(|line| muted(color).apply(line)),
        );
    }
    let (source, source_omitted) = ToolDisplayText::truncate_utf8(diff, 512 * 1024);
    let clean = TerminalText::sanitize(&source);
    let clean = clean.trim_end_matches(['\r', '\n']);
    if clean.trim().is_empty() {
        return Ok(output);
    }
    let source = clean
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect::<Vec<_>>();
    let reports = source.iter().take_while(|line| chunk_report(line)).count();
    let reports = if reports > 0 && source.get(reports) == Some(&"") {
        reports
    } else {
        0
    };
    let files = structured_diff(if reports > 0 {
        &source[reports + 1..]
    } else {
        &source
    })?;
    let rows = match files {
        Some(files) => {
            let mut rows = source[..reports]
                .iter()
                .map(|line| ((*line).to_owned(), true))
                .collect::<Vec<_>>();
            rows.extend(
                (if inline || columns < 40 {
                    inline_rows(&files)
                } else {
                    side_by_side_rows(&files, columns)
                })
                .into_iter()
                .map(|line| (line, false)),
            );
            rows
        }
        None => source
            .iter()
            .map(|line| (expand_tabs(line), false))
            .collect(),
    };
    for (line, is_muted) in rows.iter().take(MAXIMUM_DIFF_ROWS) {
        output.push(
            (if *is_muted {
                muted(color)
            } else {
                diff_style(line, color)
            })
            .apply(&TerminalText::clip(line, columns)),
        );
    }
    if rows.len() > MAXIMUM_DIFF_ROWS {
        output.push(muted(color).apply(&TerminalText::clip(
            &format!("… {} diff rows omitted", rows.len() - MAXIMUM_DIFF_ROWS),
            columns,
        )));
    } else if source_omitted {
        output.push(muted(color).apply(&TerminalText::clip(
            "… additional diff text omitted",
            columns,
        )));
    }
    Ok(output)
}
