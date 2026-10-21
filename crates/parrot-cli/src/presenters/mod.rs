//! Source-shaped enhanced terminal presenters, durable replay and live rendering.

pub mod activity;
mod factory;
mod immediate;
pub use immediate::ImmediateScrollbackValue;
mod model;
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
pub use activity::{ActivityUpdate, RawActivityView};
pub use model::*;
pub use registry::ToolPresenterRegistry;
pub use rendering::{ActivityDecoration, ToolDisplayText};
pub use replay::SessionReplayContext;
pub use session::{EnhancedSessionPresenter, PresentationUpdate};
pub use state::AgentSessionState;
pub use status::{MarqueeValue, ModelineValue};
pub use tasks::{
    AgentTaskDeclarationFormatter, AgentTaskProgressFormatter, AgentTaskRow, LiveModelAliasIcon,
    TaskAgentLine,
};
pub use turn::{EnhancedTurnRenderer, TurnPresentation};
pub use usage::*;
