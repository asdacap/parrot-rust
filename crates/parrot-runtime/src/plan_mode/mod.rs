//! Source-shaped user modes and selected-scratch plan artifacts.
//!
//! Mode profiles retain supplied immutable security snapshots. The host's real
//! ForAgent materializer and filesystem tool enforcement are separate adapters;
//! this module never interprets grants or implements the blocked security path.
mod artifacts;
mod registry;
mod workflow;

pub use artifacts::{ArtifactPaths, PlanArtifactStore, PlanTokenSource, SystemPlanTokens};
pub use registry::{
    MaterializedModeProfile, ModeRegistry, RetainedModePolicyMaterializer, SessionMode,
    SessionModeProfile, UserSessionModes,
};
pub use workflow::{
    ModeCompletionOutcome, ModeWorkflow, ModeWorkflowFactory, PlanModeWorkflow,
    PlanModeWorkflowFactory,
};

use std::{fmt, io};

pub const COORDINATOR: &str = "coordinator";
pub const BUILD: &str = "build";
pub const PLAN: &str = "plan";
pub const QUERY: &str = "query";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModeError {
    InvalidArgument(String),
    Registry(String),
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    Template(String),
}
impl ModeError {
    pub(crate) fn io(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}
impl fmt::Display for ModeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(message)
            | Self::Registry(message)
            | Self::Io { message, .. }
            | Self::Template(message) => formatter.write_str(message),
        }
    }
}
impl std::error::Error for ModeError {}

pub type ModeResult<T> = Result<T, ModeError>;
