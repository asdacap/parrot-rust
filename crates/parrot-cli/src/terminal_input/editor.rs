use super::{PromptState, TerminalKey, TerminalKeyKind};
use std::fmt;

/// A replacement range extends beyond the editor's Unicode scalar values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorRangeError;

impl fmt::Display for EditorRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("replacement range is outside the editor text")
    }
}

impl std::error::Error for EditorRangeError {}

/// A bounded editor whose cursor and replacement ranges count Unicode scalars,
/// not UTF-8 bytes or grapheme clusters. Invalid UTF-16 is not representable in
/// Rust `&str`; this API does not emulate .NET's replacement of lone surrogates.
pub struct IncrementalEditor {
    prefix: String,
    maximum_runes: usize,
    text: Vec<char>,
    history: Vec<String>,
    draft: String,
    cursor: usize,
    history_index: usize,
}

impl IncrementalEditor {
    pub fn new(prefix: impl Into<String>, maximum_runes: usize) -> Self {
        Self {
            prefix: prefix.into(),
            maximum_runes,
            text: Vec::new(),
            history: Vec::new(),
            draft: String::new(),
            cursor: 0,
            history_index: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn is_recalling(&self) -> bool {
        self.history_index > 0
    }

    pub fn prompt(&self) -> PromptState {
        PromptState {
            prefix: self.prefix.clone(),
            text: self.text.iter().collect(),
            cursor: self.cursor as isize,
        }
    }

    pub fn remember(&mut self, entry: &str) {
        if !entry.trim_matches(dotnet_whitespace).is_empty() {
            self.history.push(entry.to_owned());
        }
    }

    pub fn recall(&mut self) {
        if self.history_index < self.history.len() {
            if self.history_index == 0 {
                self.draft = self.text.iter().collect();
            }
            self.history_index += 1;
            let entry = self.history[self.history.len() - self.history_index].clone();
            self.replace_text(&entry);
        }
    }

    pub fn next(&mut self) {
        if self.history_index == 0 {
            return;
        }
        self.history_index -= 1;
        let entry = if self.history_index == 0 {
            self.draft.clone()
        } else {
            self.history[self.history.len() - self.history_index].clone()
        };
        self.replace_text(&entry);
    }

    /// Replaces text without ending history recall or changing remembered entries.
    pub fn replace_text(&mut self, value: &str) {
        self.clear();
        self.insert(value);
    }

    pub fn apply(&mut self, key: TerminalKey) -> Option<String> {
        match key.kind {
            TerminalKeyKind::Character | TerminalKeyKind::Paste => {
                self.insert(&key.text);
                self.end_recall();
            }
            TerminalKeyKind::Left => self.cursor = self.cursor.saturating_sub(1),
            TerminalKeyKind::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            TerminalKeyKind::Home => self.cursor = self.line_start(),
            TerminalKeyKind::PromptStart => self.cursor = 0,
            TerminalKeyKind::End => self.cursor = self.line_end(),
            TerminalKeyKind::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
                self.end_recall();
            }
            TerminalKeyKind::Delete if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
                self.end_recall();
            }
            TerminalKeyKind::KillLine => {
                let mut end = self.line_end();
                if end == self.cursor && end < self.text.len() {
                    end += 1;
                }
                self.text.drain(self.cursor..end);
                self.end_recall();
            }
            TerminalKeyKind::Newline => {
                self.insert("\n");
                self.end_recall();
            }
            TerminalKeyKind::EndOfFile if !self.text.is_empty() => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
                self.end_recall();
            }
            TerminalKeyKind::Submit => {
                let text: String = self.text.iter().collect();
                let submitted = text.trim_matches(dotnet_whitespace).to_owned();
                self.clear();
                self.history_index = 0;
                self.draft.clear();
                return Some(submitted);
            }
            _ => {}
        }
        None
    }

    /// Clears text and cursor while preserving history recall and its saved draft.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn replace(&mut self, value: &str) {
        self.replace_text(value);
        self.end_recall();
    }

    pub fn replace_range(
        &mut self,
        start: usize,
        length: usize,
        value: &str,
    ) -> Result<(), EditorRangeError> {
        if start > self.text.len() || length > self.text.len() - start {
            return Err(EditorRangeError);
        }
        self.text.drain(start..start + length);
        self.cursor = start;
        self.insert(value);
        self.end_recall();
        Ok(())
    }

    fn end_recall(&mut self) {
        self.history_index = 0;
    }

    fn insert(&mut self, value: &str) {
        let runes: Vec<char> = value
            .chars()
            .filter(|rune| *rune == '\n' || !rune.is_control())
            .take(self.maximum_runes - self.text.len())
            .collect();
        let accepted = runes.len();
        self.text.splice(self.cursor..self.cursor, runes);
        self.cursor += accepted;
    }

    fn line_start(&self) -> usize {
        let mut index = self.cursor;
        while index > 0 && self.text[index - 1] != '\n' {
            index -= 1;
        }
        index
    }

    fn line_end(&self) -> usize {
        let mut index = self.cursor;
        while index < self.text.len() && self.text[index] != '\n' {
            index += 1;
        }
        index
    }
}

// String.Trim uses Char.IsWhiteSpace, not ASCII whitespace or grapheme boundaries.
fn dotnet_whitespace(value: char) -> bool {
    matches!(value, '\u{0009}'..='\u{000d}' | ' ' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}'
        | '\u{202f}' | '\u{205f}' | '\u{3000}')
}
