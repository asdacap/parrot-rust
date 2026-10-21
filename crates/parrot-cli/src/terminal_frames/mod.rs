//! Inline terminal frame rendering and joined live-update lifecycles.

mod renderer;
mod scheduling;
mod surface;

pub use crate::terminal_layout::{TerminalCellStyleSpan, TerminalLine, TerminalStyle};
pub use renderer::TerminalFrameRenderer;
pub use scheduling::{
    CancellationToken, LiveUpdateScheduler, PreserveFrame, SPINNER_INTERVAL, TerminalSpinner,
    spinner_text,
};

use std::any::Any;
use std::io;
use std::sync::Arc;

/// Opaque consumer identity. Packing is decided by the item, not pointer identity.
pub type ItemIdentity = Arc<dyn Any + Send + Sync>;

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
pub struct LiveBufferRenderContext {
    pub columns: usize,
    pub color: bool,
}

pub trait LiveBufferItem: Send + Sync {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender>;
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
pub struct ScrollbackRenderContext {
    pub columns: usize,
    pub color: bool,
    pub inline_diff: bool,
    pub previous_packing: Option<ItemIdentity>,
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
    fn sequence_identity(&self) -> Option<ItemIdentity> {
        None
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        None
    }
    fn packs(&self, _previous: Option<&ItemIdentity>) -> bool {
        false
    }
    fn continues(&self, previous: &dyn ScrollbackItem) -> bool;
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalFrame {
    pub lines: Vec<TerminalLine>,
    pub caret: LiveBufferCaret,
}
