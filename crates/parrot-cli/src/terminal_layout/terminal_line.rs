use std::hash::{Hash, Hasher};

/// A terminal style escape prefix. Empty strings represent no styling.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct TerminalStyle {
    start: String,
}

impl TerminalStyle {
    pub const RESET: &'static str = "\x1b[0m";

    pub fn new(start: impl Into<String>) -> Self {
        Self {
            start: start.into(),
        }
    }

    pub fn start(&self) -> &str {
        &self.start
    }

    pub fn apply(&self, value: &str) -> String {
        if self.start.is_empty() {
            value.to_owned()
        } else {
            format!("{}{}{}", self.start, value, Self::RESET)
        }
    }
}

/// A style override over a range of terminal cells.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TerminalCellStyleSpan {
    pub start_cell: usize,
    pub length: usize,
    pub style: TerminalStyle,
}

impl TerminalCellStyleSpan {
    pub fn new(start_cell: usize, length: usize, style: TerminalStyle) -> Self {
        Self {
            start_cell,
            length,
            style,
        }
    }
}

/// Immutable rendered terminal text with its base style and cell-style spans.
#[derive(Clone, Debug)]
pub struct TerminalLine {
    text: String,
    style: TerminalStyle,
    style_spans: Vec<TerminalCellStyleSpan>,
}

impl TerminalLine {
    pub fn new(text: impl Into<String>, style: TerminalStyle) -> Self {
        Self::with_spans(text, style, Vec::new())
    }

    pub fn with_spans(
        text: impl Into<String>,
        style: TerminalStyle,
        style_spans: impl Into<Vec<TerminalCellStyleSpan>>,
    ) -> Self {
        Self {
            text: text.into(),
            style,
            style_spans: style_spans.into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn style(&self) -> &TerminalStyle {
        &self.style
    }

    pub fn style_spans(&self) -> &[TerminalCellStyleSpan] {
        &self.style_spans
    }
}

impl PartialEq for TerminalLine {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.style == other.style
            && self.style_spans == other.style_spans
    }
}

impl Eq for TerminalLine {}

impl Hash for TerminalLine {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        self.style.hash(state);
        self.style_spans.hash(state);
    }
}
