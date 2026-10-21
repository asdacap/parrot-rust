use unicode_segmentation::UnicodeSegmentation;

use crate::terminal_layout::{TerminalStyle, TerminalText};

pub(super) struct TerminalSurface {
    width: usize,
    height: usize,
    characters: Vec<String>,
    styles: Vec<TerminalStyle>,
}

impl TerminalSurface {
    pub(super) fn new(width: usize, height: usize) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            characters: vec![" ".into(); width * height],
            styles: vec![TerminalStyle::default(); width * height],
        }
    }

    pub(super) fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width != width || self.height != height {
            *self = Self::new(width, height);
        }
    }

    pub(super) fn clear(&mut self) {
        self.characters.fill(" ".into());
        self.styles.fill(TerminalStyle::default());
    }

    pub(super) fn write(&mut self, row: usize, value: &str, style: &TerminalStyle) {
        if row >= self.height {
            return;
        }
        let mut column = 0;
        for grapheme in value.graphemes(true) {
            let width = TerminalText::width(grapheme);
            if width == 0 {
                if column > 0 {
                    self.characters[row * self.width + column - 1].push_str(grapheme);
                }
                continue;
            }
            if column + width > self.width {
                break;
            }
            let index = row * self.width + column;
            self.characters[index] = grapheme.into();
            self.styles[index] = style.clone();
            for occupied in 1..width {
                self.characters[index + occupied].clear();
                self.styles[index + occupied] = style.clone();
            }
            column += width;
        }
    }

    pub(super) fn apply_style(
        &mut self,
        row: usize,
        start: usize,
        length: usize,
        style: &TerminalStyle,
    ) {
        if row >= self.height {
            return;
        }
        let end = start.saturating_add(length).min(self.width);
        for column in start.min(self.width)..end {
            self.styles[row * self.width + column] = style.clone();
        }
    }

    pub(super) fn render_row(&self, row: usize, background: &TerminalStyle) -> String {
        let mut rendered = background.start().to_owned();
        let mut active = background;
        for column in 0..self.width {
            let index = row * self.width + column;
            let cell = &self.styles[index];
            let style = if cell.start().is_empty() {
                background
            } else {
                cell
            };
            if active != style {
                if !active.start().is_empty() {
                    rendered.push_str(TerminalStyle::RESET);
                }
                rendered.push_str(style.start());
                active = style;
            }
            rendered.push_str(&self.characters[index]);
        }
        if !active.start().is_empty() {
            rendered.push_str(TerminalStyle::RESET);
        }
        rendered
    }
}
