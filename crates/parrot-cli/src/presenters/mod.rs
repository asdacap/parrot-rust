//! Source-shaped enhanced terminal presenters, durable replay and live rendering.

pub mod activity;
mod factory;
mod immediate;
pub use immediate::ImmediateScrollbackValue;
mod model;
mod progress;
mod reasoning;
mod registry;
pub mod rendering;
mod replay;
mod session;
mod state;
pub mod status;
pub mod streams;
pub mod tasks;
pub mod tools;
mod turn;
pub mod usage;

pub use crate::dispatch::hierarchy::AgentSessionHierarchy;

pub(crate) fn duration_text(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}
pub use activity::{ActivityUpdate, RawActivityView};
pub use model::*;
pub use registry::ToolPresenterRegistry;
pub use rendering::{ActivityDecoration, ToolDisplayText};
pub use replay::SessionReplayState;
pub use session::{EnhancedSessionPresenter, PresentationUpdate};
pub use state::AgentSessionState;
pub use status::{MarqueeValue, ModelineValue};
pub use tasks::{
    AgentTaskDeclarationFormatter, AgentTaskProgressFormatter, AgentTaskRow, LiveModelAliasIcon,
    TaskAgentLine,
};
pub use turn::{EnhancedTurnRenderer, TurnPresentation};
pub use usage::*;
