use super::ActivityDecoration;
use crate::terminal_layout::{MarkdownRenderer, TerminalText};

#[derive(Clone)]
pub(super) enum ReasoningEntry {
    Summary(String),
    Counted(String),
    Encrypted,
}

impl ReasoningEntry {
    fn render(&self, columns: usize, decoration: &ActivityDecoration) -> Vec<String> {
        let content_columns = decoration.content_columns(columns);
        let content = match self {
            Self::Summary(markdown) => {
                MarkdownRenderer::render("", markdown, content_columns, false)
            }
            Self::Counted(tokens) => TerminalText::layout_words(
                &format!("Reasoned for {tokens} tokens…"),
                content_columns,
            ),
            Self::Encrypted => {
                TerminalText::layout_words("Reasoning received (encrypted)", content_columns)
            }
        };
        decoration.apply("✦", content)
    }
}

#[derive(Clone)]
pub(crate) struct ReasoningRow {
    owner: String,
    depth: usize,
    label: Option<String>,
    entries: Vec<ReasoningEntry>,
}

impl ReasoningRow {
    pub(super) fn new(owner: String, depth: usize, label: Option<String>) -> Self {
        Self {
            owner,
            depth,
            label,
            entries: Vec::new(),
        }
    }

    pub(super) fn owner(&self) -> &str {
        &self.owner
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn append(&mut self, entry: ReasoningEntry, columns: usize) -> Vec<Self> {
        let mut flushed = Vec::new();
        let decoration =
            ActivityDecoration::describe(columns, self.depth, self.label.as_deref(), "");
        let rendered = entry.render(columns, &decoration);
        let continuation = entry.render(columns, &ActivityDecoration::default());
        let current = self.render(columns);
        let fits = rendered.len() == 1
            && TerminalText::width(&rendered[0]) <= columns
            && (current.is_empty()
                || (current.len() == 1
                    && continuation.len() == 1
                    && TerminalText::width(&current[0])
                        + 1
                        + TerminalText::width(&continuation[0])
                        <= columns));
        if !fits && !self.is_empty() {
            flushed.push(self.drain());
        }
        self.entries.push(entry);
        if rendered.len() != 1 || TerminalText::width(&rendered[0]) > columns {
            flushed.push(self.drain());
        }
        flushed
    }

    pub(super) fn repack(&mut self, columns: usize) -> Vec<Self> {
        let entries = std::mem::take(&mut self.entries);
        let mut flushed = Vec::new();
        for entry in entries {
            flushed.extend(self.append(entry, columns));
        }
        flushed
    }

    fn drain(&mut self) -> Self {
        Self {
            owner: self.owner.clone(),
            depth: self.depth,
            label: self.label.clone(),
            entries: std::mem::take(&mut self.entries),
        }
    }

    pub(super) fn render(&self, columns: usize) -> Vec<String> {
        let decoration =
            ActivityDecoration::describe(columns, self.depth, self.label.as_deref(), "");
        let mut lines = Vec::new();
        let mut pending = String::new();
        for entry in &self.entries {
            let rendered = entry.render(columns, &decoration);
            if rendered.len() == 1 && TerminalText::width(&rendered[0]) <= columns {
                let next = &rendered[0];
                let continuation = entry.render(columns, &ActivityDecoration::default());
                if !pending.is_empty()
                    && continuation.len() == 1
                    && TerminalText::width(&pending) + 1 + TerminalText::width(&continuation[0])
                        <= columns
                {
                    pending.push(' ');
                    pending.push_str(&continuation[0]);
                } else {
                    if !pending.is_empty() {
                        lines.push(std::mem::take(&mut pending));
                    }
                    pending.clone_from(next);
                }
            } else {
                if !pending.is_empty() {
                    lines.push(std::mem::take(&mut pending));
                }
                lines.extend(rendered);
            }
        }
        if !pending.is_empty() {
            lines.push(pending);
        }
        lines
    }
}
