//! Live-only status values. Protocol data is sanitized before terminal output.

use std::io;

use unicode_segmentation::UnicodeSegmentation;

use crate::terminal_frames::{
    LiveBufferItem, LiveBufferRender, LiveBufferRenderContext, LiveBufferRetention, TerminalLine,
    TerminalStyle,
};
use crate::terminal_layout::TerminalText;

#[derive(Clone, Debug)]
pub struct MarqueeValue {
    pub prefix: String,
    pub text: String,
    pub frame: usize,
}

impl LiveBufferItem for MarqueeValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let prefix = TerminalText::sanitize_line(&self.prefix);
        let text = TerminalText::sanitize_line(&self.text);
        let available = context.columns.saturating_sub(TerminalText::width(&prefix));
        let value = if available == 0 {
            TerminalText::clip(&prefix, context.columns)
        } else {
            format!("{prefix}{}", viewport(&text, available, self.frame))
        };
        Ok(LiveBufferRender {
            lines: vec![TerminalLine::new(
                value,
                TerminalStyle::new(if context.color {
                    "\x1b[48;5;236m\x1b[38;5;252m"
                } else {
                    ""
                }),
            )],
            caret: None,
            retention: LiveBufferRetention::Tail,
        })
    }
}

fn viewport(text: &str, width: usize, frame: usize) -> String {
    let mut cells: Vec<Option<String>> = Vec::new();
    for grapheme in text.graphemes(true) {
        let count = TerminalText::width(grapheme);
        if count == 0 {
            if let Some(Some(previous)) = cells.last_mut() {
                previous.push_str(grapheme);
            }
        } else {
            cells.push(Some(grapheme.to_owned()));
            cells.extend((1..count).map(|_| None));
        }
    }
    if cells.len() <= width {
        return text.to_owned();
    }
    cells.extend((0..3).map(|_| Some(" ".into())));
    let offset = frame % cells.len();
    let mut result = String::new();
    let mut column = 0;
    while column < width {
        match &cells[(offset + column) % cells.len()] {
            None => {
                result.push(' ');
                column += 1;
            }
            Some(value) => {
                let count = TerminalText::width(value);
                if count > width - column {
                    result.extend(std::iter::repeat_n(' ', width - column));
                    break;
                }
                result.push_str(value);
                column += count;
            }
        }
    }
    result
}

#[derive(Clone, Debug, Default)]
pub struct ModelineValue {
    pub mode: String,
    pub activity: String,
    pub model: String,
}

impl ModelineValue {
    pub fn render_text(&self, width: usize) -> String {
        let columns = width.saturating_sub(1).max(1);
        if columns < 3 {
            return "─".repeat(columns);
        }
        let left = TerminalText::sanitize(&self.mode);
        let left = if left.is_empty() {
            left
        } else {
            format!("mode: {left}")
        };
        let center = TerminalText::sanitize(&self.activity);
        let right = TerminalText::sanitize(&self.model);
        let labels: Vec<_> = [&left, &center]
            .into_iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect();
        let left_part = if labels.is_empty() {
            String::new()
        } else {
            format!("─ {} ", labels.join(" ─ "))
        };
        if labels.is_empty() && right.is_empty() {
            return "─".repeat(columns);
        }
        if right.is_empty() {
            if TerminalText::width(&left_part) >= columns {
                return format!("{}─", TerminalText::clip(&left_part, columns - 1));
            }
            return format!(
                "{left_part}{}",
                "─".repeat(columns - TerminalText::width(&left_part))
            );
        }
        let right_part = format!(" {right} ");
        if TerminalText::width(&right_part) >= columns {
            return format!("─{}", TerminalText::clip(&right_part, columns - 1));
        }
        let left_width =
            TerminalText::width(&left_part).min(columns - TerminalText::width(&right_part));
        format!(
            "{}{}{right_part}",
            TerminalText::clip(&left_part, left_width),
            "─".repeat(columns - left_width - TerminalText::width(&right_part))
        )
    }
}

/// Selected countdown display only; T72 owns the deadline and answer lifetime.
pub struct QuestionCountdownValue(pub i64);

impl LiveBufferItem for QuestionCountdownValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let text = format!("Auto-return in {}:{:02}", self.0 / 60, self.0 % 60);
        Ok(LiveBufferRender {
            lines: TerminalText::layout_words(&text, context.columns)
                .into_iter()
                .map(|line| {
                    TerminalLine::new(
                        line,
                        TerminalStyle::new(if context.color {
                            "\x1b[48;5;236m\x1b[38;5;252m"
                        } else {
                            ""
                        }),
                    )
                })
                .collect(),
            caret: None,
            retention: LiveBufferRetention::Fixed,
        })
    }
}

impl LiveBufferItem for ModelineValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        Ok(LiveBufferRender {
            lines: vec![TerminalLine::new(
                self.render_text(context.columns),
                TerminalStyle::new(if context.color {
                    "\x1b[48;5;236m\x1b[32m"
                } else {
                    ""
                }),
            )],
            caret: None,
            retention: LiveBufferRetention::Fixed,
        })
    }
}
