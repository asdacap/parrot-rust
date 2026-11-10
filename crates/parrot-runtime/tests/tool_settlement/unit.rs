use super::{RuntimeTools, State, TurnBinding};
use crate as parrot_runtime;
use crate as runtime_crate;

mod fixture;
use fixture::*;
mod common;
use common::*;
mod harness;
use harness::*;
mod admission;
mod budget_scope;
mod crash_process;
mod focused;
mod images;
mod owned_recovery;
mod parallel_images;
#[path = "runtime_support.rs"]
mod runtime;
mod settlement;
mod snapshot;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolExecutionChunk {
    pub assistant_sequence: i64,
    pub starts_batch: bool,
    pub calls: Vec<parrot_core::ToolCall>,
    pub cancel_remaining: bool,
}

use crate::agent_turn::usage::SessionUsageRecorder;
use parrot_storage::{artifacts::ImageArtifactMetadata, events::ToolExecutionTerminal};

use parrot_storage::conversation::AgentConversation;
