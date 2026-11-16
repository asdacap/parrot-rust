//! Inline terminal frame rendering.

mod renderer;
mod scheduling;
mod surface;

pub use crate::terminal_layout::{TerminalCellStyleSpan, TerminalLine, TerminalStyle};
pub use renderer::TerminalFrameRenderer;
pub use scheduling::{SPINNER_FRAMES, SPINNER_INTERVAL, spinner_text};

use std::io;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveBufferCaret {
    pub row: isize,
    pub cells: isize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveBufferRetention {
    Tail,
    Fixed,
    Caret,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveBufferRender {
    pub lines: Vec<TerminalLine>,
    pub caret: Option<LiveBufferCaret>,
    pub retention: LiveBufferRetention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveBufferRenderOptions {
    pub columns: usize,
    pub color: bool,
}

pub trait LiveBufferItem: Send + Sync {
    fn render(&self, options: LiveBufferRenderOptions) -> io::Result<LiveBufferRender>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScrollbackLayout {
    #[default]
    Compact,
    Block,
    User,
    Assistant,
}

#[derive(Clone)]
pub struct ScrollbackRenderOptions {
    pub columns: usize,
    pub color: bool,
    pub inline_diff: bool,
    pub previous_item: Option<Arc<dyn ScrollbackItem>>,
}

pub trait ScrollbackItem: Send + Sync {
    fn is_completed(&self) -> bool;
    fn layout(&self) -> ScrollbackLayout {
        ScrollbackLayout::Compact
    }
    fn starts_layout(&self) -> bool {
        true
    }
    fn ends_layout(&self) -> bool {
        true
    }
    /// Packing stream of the item. Consecutive non-empty items with an equal
    /// first component pack together, and an equal second component suppresses
    /// the repeated heading. `None` for items that never pack.
    fn stream(&self) -> Option<(&str, &str)> {
        None
    }
    fn packs(&self, previous: Option<&dyn ScrollbackItem>) -> bool {
        self.stream()
            .zip(previous.and_then(|previous| previous.stream()))
            .is_some_and(|(item, previous)| item.0 == previous.0)
    }
    fn continues(&self, previous: &dyn ScrollbackItem) -> bool;
    fn render(&self, options: ScrollbackRenderOptions) -> io::Result<Vec<String>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalFrame {
    pub lines: Vec<TerminalLine>,
    pub caret: LiveBufferCaret,
}
