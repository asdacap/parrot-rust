//! Incremental terminal key decoding, rune-indexed editing and Unix raw input.

mod decoder;
mod editor;
pub mod native;

pub use decoder::TerminalKeyDecoder;
pub use editor::{EditorRangeError, IncrementalEditor};
pub use native::{RawTerminal, ReadResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalKeyKind {
    Character,
    Paste,
    Left,
    Right,
    Up,
    Down,
    Home,
    PromptStart,
    End,
    Backspace,
    Delete,
    KillLine,
    Submit,
    Newline,
    Complete,
    Escape,
    Interrupt,
    EndOfFile,
    Mode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalKey {
    pub kind: TerminalKeyKind,
    pub text: String,
}

impl TerminalKey {
    pub fn new(kind: TerminalKeyKind) -> Self {
        Self::with_text(kind, "")
    }

    pub fn with_text(kind: TerminalKeyKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Cursor offsets count Unicode scalar values, matching the source's Rune list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptState {
    pub prefix: String,
    pub text: String,
    pub cursor: isize,
}

impl PromptState {
    pub fn sanitize(&self) -> Self {
        let text = sanitize_text(&self.text);
        Self {
            prefix: sanitize_text(&self.prefix).replace('\n', ""),
            cursor: self.cursor.clamp(0, text.chars().count() as isize),
            text,
        }
    }
}

fn sanitize_text(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        if ch == '\t' {
            result.push_str("    ");
        } else if ch == '\n' || !ch.is_control() {
            result.push(ch);
        }
    }
    result
}
