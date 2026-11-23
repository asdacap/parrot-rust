//! Leaf response validation of task payload sessions at the completion blocker.
//! A blocked exit keeps the turn open so the worker corrects its own final
//! response; the runner only parses once the turn actually closes.
use crate::agent_turn::services::{CompletionStateSource, SelectionSource, TurnError};
use parrot_domain::contracts::{
    agent_tasks::TaskArtifactParser,
    async_ops::{BoxFuture, Cancellation},
    security::AccessSnapshot,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The profile id the task scheduler requests for dedicated payload workers.
pub const TASK_PAYLOAD_PROFILE: &str = "agent-task-payload";

/// Blocks a task payload worker's exit while its candidate final response fails
/// the leaf response contract, up to the configured repair budget per turn.
pub struct LeafResponseBlocker<S: AccessSnapshot, P: TaskArtifactParser + 'static> {
    pub selection: Arc<dyn SelectionSource<S>>,
    pub parser: Arc<P>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub maximum_response_repairs: u64,
    blocked: AtomicU64,
}

impl<S: AccessSnapshot + 'static, P: TaskArtifactParser + 'static> LeafResponseBlocker<S, P> {
    pub fn new(
        selection: Arc<dyn SelectionSource<S>>,
        parser: Arc<P>,
        templates: Arc<PromptTemplateCatalog>,
        maximum_response_repairs: u64,
    ) -> Self {
        Self {
            selection,
            parser,
            templates,
            maximum_response_repairs,
            blocked: AtomicU64::new(0),
        }
    }
}

impl<S: AccessSnapshot + 'static, P: TaskArtifactParser + 'static> CompletionStateSource
    for LeafResponseBlocker<S, P>
{
    fn get_task_leaf_reminder<'a>(
        &'a self,
        candidate: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            let selection = self.selection.capture()?;
            if selection.profile.id != TASK_PAYLOAD_PROFILE {
                return Ok(None);
            }
            let error = match self.parser.parse_leaf_response(candidate) {
                Ok(_) => return Ok(None),
                Err(error) => error,
            };
            let blocked = self.blocked.fetch_add(1, Ordering::SeqCst);
            if blocked >= self.maximum_response_repairs {
                return Ok(None);
            }
            self.templates
                .render(
                    "agent-task.response-repair",
                    &[("error".into(), error.to_string().into())],
                    cancellation,
                )
                .map(Some)
                .map_err(|error| TurnError::Service(error.to_string()))
        })
    }
    fn begin_turn(&self) {
        self.blocked.store(0, Ordering::SeqCst);
    }
    fn get_pending_ask_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
    fn get_active_work_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
    fn get_exit_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
}
