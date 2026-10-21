use std::io;

use crate::terminal_frames::{
    ScrollbackItem, ScrollbackLayout, ScrollbackRenderContext, TerminalStyle,
};
use crate::terminal_layout::TerminalText;

/// Source immediate scrollback values, shared by real input and rendering consumers.
pub struct ImmediateScrollbackValue {
    lines: Vec<String>,
    muted: bool,
    layout: ScrollbackLayout,
}

impl ImmediateScrollbackValue {
    pub fn trusted(lines: Vec<String>) -> Self {
        Self {
            lines,
            muted: false,
            layout: ScrollbackLayout::Compact,
        }
    }
    pub fn muted(lines: Vec<String>) -> Self {
        Self {
            lines,
            muted: true,
            layout: ScrollbackLayout::Compact,
        }
    }
    pub fn user(text: &str) -> Self {
        let clean = TerminalText::sanitize(text);
        let text = clean.strip_prefix("› ").unwrap_or(&clean).to_owned();
        Self {
            lines: vec![text],
            muted: false,
            layout: ScrollbackLayout::User,
        }
    }
    pub fn assistant(text: &str) -> Self {
        Self {
            lines: vec![text.into()],
            muted: false,
            layout: ScrollbackLayout::Assistant,
        }
    }
}

impl ScrollbackItem for ImmediateScrollbackValue {
    fn is_completed(&self) -> bool {
        true
    }
    fn layout(&self) -> ScrollbackLayout {
        self.layout
    }
    fn continues(&self, _previous: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        match self.layout {
            ScrollbackLayout::User | ScrollbackLayout::Assistant => {
                let user = self.layout == ScrollbackLayout::User;
                let prefix = if user { "◆ " } else { "● " };
                let clean = TerminalText::sanitize(&self.lines[0]);
                let clean = clean.trim_end_matches(['\r', '\n']);
                let style = TerminalStyle::new(if !context.color {
                    ""
                } else if user {
                    "\x1b[32m\x1b[1m"
                } else {
                    "\x1b[38;5;195m"
                });
                Ok(TerminalText::layout_words_hanging(
                    &format!("{prefix}{clean}"),
                    context.columns,
                    &" ".repeat(TerminalText::width(prefix)),
                )
                .into_iter()
                .map(|line| style.apply(&line))
                .collect())
            }
            _ if self.muted => {
                let style = TerminalStyle::new(if context.color { "\x1b[38;5;245m" } else { "" });
                Ok(self
                    .lines
                    .iter()
                    .map(|line| style.apply(&TerminalText::sanitize(line)))
                    .collect())
            }
            _ => Ok(self.lines.clone()),
        }
    }
}
