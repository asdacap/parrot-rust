//! Enhanced terminal dialogs and session-bound input lifetimes.

pub mod dialog;
mod enhanced;
pub mod interactions;
pub mod navigation;
mod terminal;

pub use dialog::{CommitScrollback, EnhancedSlashDialog, LiveInputHost};
pub use enhanced::{EnhancedChatRequest, EnhancedCli, EnhancedFailure, PreparePrompt};
pub use navigation::{
    ExplicitRemoteTerminalSessionNavigation, LocalTerminalSessionNavigation,
    TerminalSessionController, TerminalSessionNavigation, TerminalSessionTarget,
};
pub use terminal::{EnhancedTerminal, TerminalDimensions, TerminalRead};
