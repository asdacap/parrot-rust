use super::{
    CompactionGroup, CompactionPlanner, CompactionSettings, ContextSize, PlanningError,
    PlanningInput, is_eligible_for_spill,
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::{CompactionPhase, CompactionProgressEvent},
    values::config::ModelProfiles,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    routing::{aliases::ResolvedModelSelection, registry::ProviderModel},
    sessions::{self, Content, Event, Message, ProviderSessions, Request, Role},
};
use parrot_storage::artifacts::compaction::{self as artifact, CompactionGroupBlobStore};
use std::{fmt, sync::Arc};

/// Compaction progress and retry publication are awaited before execution continues.
pub trait CompactionObserver: Send + Sync {
    /// Publish a summary-provider retry before consuming its next event.
    fn observe<'a>(
        &'a self,
        event: &'a Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>>;

    /// Publish aggregate progress without exposing summary content.
    fn observe_progress<'a>(
        &'a self,
        progress: CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>>;
}

#[derive(Debug)]
pub enum CompactionError {
    Planning(PlanningError),
    Provider(sessions::Error),
    Blob(artifact::CompactionError),
    Template(String),
    InvalidOperation(String),
    Cancelled,
}
impl CompactionError {
    /// Only the concrete top-level provider failure supplies a response body.
    pub fn response_body(&self) -> &str {
        match self {
            Self::Provider(error) => error.response_body(),
            _ => "",
        }
    }
}
impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planning(error) => error.fmt(formatter),
            Self::Provider(error) => error.fmt(formatter),
            Self::Blob(error) => error.fmt(formatter),
            Self::Template(message) | Self::InvalidOperation(message) => {
                formatter.write_str(message)
            }
            Self::Cancelled => formatter.write_str("The operation was canceled."),
        }
    }
}
impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Planning(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::Blob(error) => Some(error),
            _ => None,
        }
    }
}
impl From<PlanningError> for CompactionError {
    fn from(error: PlanningError) -> Self {
        Self::Planning(error)
    }
}
impl From<sessions::Error> for CompactionError {
    fn from(error: sessions::Error) -> Self {
        Self::Provider(error)
    }
}
impl From<artifact::CompactionError> for CompactionError {
    fn from(error: artifact::CompactionError) -> Self {
        Self::Blob(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    pub history: Vec<Message>,
    pub summary: Message,
    pub retained_messages: usize,
    pub watermark: i64,
}

struct Uncancelled;
impl Cancellation for Uncancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// Executes a planned safe prefix. The caller owns and joins provider-session lifetime
/// and commits the returned summary/watermark through authoritative history storage.
pub struct Compactor {
    planner: Arc<CompactionPlanner>,
    templates: Arc<PromptTemplateCatalog>,
    summary_instructions: String,
    summary_prefix: String,
}
impl Compactor {
    pub fn new(
        mut settings: CompactionSettings,
        profiles: ModelProfiles,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, CompactionError> {
        let render = |id| {
            templates
                .render(id, &[], &Uncancelled)
                .map_err(|error| CompactionError::Template(error.to_string()))
        };
        settings.summary_instructions = render("compaction.summary-instructions")?;
        settings.summary_prefix = render("compaction.summary-prefix")?;
        Ok(Self {
            summary_instructions: settings.summary_instructions.clone(),
            summary_prefix: settings.summary_prefix.clone(),
            planner: Arc::new(CompactionPlanner::new(settings, profiles)),
            templates,
        })
    }

    pub fn planner(&self) -> &CompactionPlanner {
        &self.planner
    }

    pub fn planner_handle(&self) -> Arc<CompactionPlanner> {
        self.planner.clone()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn compact(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
        input: &PlanningInput<'_>,
        blobs: &mut CompactionGroupBlobStore,
        sessions: &ProviderSessions,
        observer: &dyn CompactionObserver,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        let model = &selected.canonical_model;
        let target_budget = self.planner.resolve_policy(selected, target).target_tokens;
        let Some(plan) = self.planner.plan_retention(model, target_budget, input)? else {
            return Ok(None);
        };
        let pending = &input.groups[..plan.keep_group_from];
        let mut provider_groups = Vec::with_capacity(pending.len());
        let mut substituted = vec![false; pending.len()];
        for (index, group) in pending.iter().enumerate() {
            let messages =
                if self
                    .planner
                    .estimate_summary_request_tokens(model, "", 0, group.messages)?
                    > plan.input_budget
                {
                    if !is_eligible_for_spill(group) {
                        return Err(PlanningError::GroupExceedsBudget.into());
                    }
                    substituted[index] = true;
                    let notice = self.spill(group, blobs, cancellation)?;
                    if self
                        .planner
                        .estimate_summary_request_tokens(model, "", 0, &notice)?
                        > plan.input_budget
                    {
                        return Err(PlanningError::GroupExceedsBudget.into());
                    }
                    notice
                } else {
                    group.messages.to_vec()
                };
            provider_groups.push(messages);
        }

        let runner = SummaryRunner {
            compactor: self,
            selected: model,
            sessions,
            observer,
            cancellation,
            maximum_output_tokens: plan.summary_output_tokens,
            total_groups: pending.len(),
        };
        let mut completed_groups = 0;
        runner.report_progress(completed_groups).await?;
        let mut summary = String::new();
        let mut chunk = Vec::new();
        let mut chunk_tokens = 0;
        for (index, mut messages) in provider_groups.into_iter().enumerate() {
            if !chunk.is_empty()
                && self.planner.estimate_summary_request_tokens(
                    model,
                    &summary,
                    chunk_tokens,
                    &messages,
                )? > plan.input_budget
            {
                summary = runner
                    .fold_groups(&summary, &chunk, &mut completed_groups)
                    .await?;
                chunk.clear();
                chunk_tokens = 0;
            }
            if self.planner.estimate_summary_request_tokens(
                model,
                &summary,
                chunk_tokens,
                &messages,
            )? > plan.input_budget
            {
                if !is_eligible_for_spill(&pending[index]) || substituted[index] {
                    return Err(PlanningError::GroupExceedsBudget.into());
                }
                messages = self.spill(&pending[index], blobs, cancellation)?;
                substituted[index] = true;
                if self.planner.estimate_summary_request_tokens(
                    model,
                    &summary,
                    chunk_tokens,
                    &messages,
                )? > plan.input_budget
                {
                    return Err(PlanningError::GroupExceedsBudget.into());
                }
            }
            chunk_tokens = chunk_tokens
                .checked_add(CompactionPlanner::estimate_tokens(model, &messages)?)
                .ok_or(PlanningError::TokenOverflow)?;
            chunk.push(messages);
        }
        if !chunk.is_empty() {
            summary = runner
                .fold_groups(&summary, &chunk, &mut completed_groups)
                .await?;
        }
        let summary = Message::system(format!("{}{summary}", self.summary_prefix));
        let retained: Vec<_> = input.groups[plan.keep_group_from..]
            .iter()
            .flat_map(|group| group.messages.iter().cloned())
            .collect();
        let mut history = vec![summary.clone(), input.fixed_message.clone()];
        let retained_messages = retained.len();
        history.extend(retained);
        if CompactionPlanner::estimate_input_tokens(
            model,
            input.instructions,
            input.tools,
            &history,
        )? > i64::from(model.model.input_token_limit())
        {
            return Err(CompactionError::InvalidOperation(
                "The compacted conversation exceeds the selected model input limit.".into(),
            ));
        }
        Ok(Some(CompactionResult {
            history,
            summary,
            retained_messages,
            watermark: plan.watermark,
        }))
    }

    fn spill(
        &self,
        group: &CompactionGroup<'_>,
        blobs: &mut CompactionGroupBlobStore,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<Message>, CompactionError> {
        let group = artifact::CompactionGroup {
            messages: group.messages.iter().map(artifact_message).collect(),
        };
        let path = blobs.persist(&group, cancellation)?;
        let notice = self
            .templates
            .render(
                "compaction.oversized-tool-group-notice",
                &[("path".into(), path.to_string_lossy().as_ref().into())],
                &Uncancelled,
            )
            .map_err(|error| CompactionError::Template(error.to_string()))?;
        Ok(vec![Message::system(notice)])
    }

    fn request_messages(&self, preceding_summary: &str, chunk: &[Message]) -> Vec<Message> {
        let mut messages = vec![Message::system(&self.summary_instructions)];
        if !preceding_summary.is_empty() {
            messages.push(Message::system(format!(
                "{}{preceding_summary}",
                self.summary_prefix
            )));
        }
        messages.extend_from_slice(chunk);
        messages
    }
}

enum SummaryOutcome {
    Summary(String),
    NoSummary(String),
}

struct SummaryRunner<'a> {
    compactor: &'a Compactor,
    selected: &'a ProviderModel,
    sessions: &'a ProviderSessions,
    observer: &'a dyn CompactionObserver,
    cancellation: &'a dyn Cancellation,
    maximum_output_tokens: i32,
    total_groups: usize,
}
impl SummaryRunner<'_> {
    async fn report_progress(&self, completed_groups: usize) -> Result<(), CompactionError> {
        self.cancellation
            .check()
            .map_err(|_| CompactionError::Cancelled)?;
        self.observer
            .observe_progress(
                CompactionProgressEvent {
                    phase: CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                    completed_groups: completed_groups as u64,
                    total_groups: self.total_groups as u64,
                    ..CompactionProgressEvent::new()
                },
                self.cancellation,
            )
            .await?;
        Ok(())
    }

    /// Empty completions split only at group boundaries; each recursive branch
    /// strictly shrinks until a single empty group fails without discarding it.
    fn fold_groups<'a>(
        &'a self,
        preceding_summary: &'a str,
        pending: &'a [Vec<Message>],
        completed_groups: &'a mut usize,
    ) -> BoxFuture<'a, Result<String, CompactionError>> {
        Box::pin(async move {
            let messages: Vec<_> = pending.iter().flatten().cloned().collect();
            match self.summarise(preceding_summary, &messages).await? {
                SummaryOutcome::Summary(summary) => {
                    *completed_groups += pending.len();
                    self.report_progress(*completed_groups).await?;
                    return Ok(summary);
                }
                SummaryOutcome::NoSummary(reason) if pending.len() == 1 => {
                    return Err(CompactionError::InvalidOperation(reason));
                }
                SummaryOutcome::NoSummary(_) => {}
            }
            let half = pending.len().div_ceil(2);
            let first = self
                .fold_groups(preceding_summary, &pending[..half], completed_groups)
                .await?;
            self.fold_groups(&first, &pending[half..], completed_groups)
                .await
        })
    }

    async fn summarise(
        &self,
        preceding_summary: &str,
        chunk: &[Message],
    ) -> Result<SummaryOutcome, CompactionError> {
        self.cancellation
            .check()
            .map_err(|_| CompactionError::Cancelled)?;
        let request = Request {
            model: self.selected.model.id.clone(),
            max_output_tokens: self.maximum_output_tokens,
            messages: self.compactor.request_messages(preceding_summary, chunk),
            reasoning: self.selected.reasoning(),
            ..Request::default()
        };
        let session = self.sessions.get(self.selected.provider.clone()).await?;
        let mut stream = session.call(request)?;
        let result = async {
            let mut summary = None;
            let mut completion_details = None;
            let mut reasoning_bytes = 0;
            while let Some(event) = stream.next(self.cancellation).await? {
                if summary.is_some() {
                    return Err(CompactionError::InvalidOperation(
                        "The compaction provider emitted an event after completion.".into(),
                    ));
                }
                match event.base_event() {
                    Event::Retry { .. } => self.observer.observe(&event, self.cancellation).await?,
                    Event::Completed(completion) if !completion.assistant_text.is_empty() => {
                        summary = Some(completion.assistant_text.clone());
                    }
                    Event::Completed(completion) => {
                        completion_details = Some(format!(
                            "The compaction provider completed with empty assistant text (finish reason: {:?}; assistant text length: {} UTF-8 bytes; provider input tokens: {}; provider output tokens: {}).",
                            completion.finish_reason,
                            completion.assistant_text.len(),
                            completion.input_tokens,
                            completion.output_tokens,
                        ));
                    }
                    Event::ReasoningDelta { text, .. } => reasoning_bytes += text.len(),
                    _ => {}
                }
            }
            Ok(match summary {
                Some(summary) => SummaryOutcome::Summary(summary),
                None => SummaryOutcome::NoSummary(format!(
                    "{} Reasoning delta length: {reasoning_bytes} UTF-8 bytes.",
                    completion_details.unwrap_or_else(|| {
                        "The compaction provider stream ended without a Completed event.".into()
                    }),
                )),
            })
        }
        .await;
        // Like await-foreach disposal, close even on cancellation, observer or
        // protocol failure; the session pool itself remains caller-owned.
        stream.close().await?;
        result
    }
}

fn artifact_message(message: &Message) -> artifact::CompactionMessage {
    artifact::CompactionMessage {
        role: match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        }
        .into(),
        contents: message
            .contents()
            .iter()
            .map(|content| match content {
                Content::Text(text) => artifact::CompactionContent {
                    kind: "Text".into(),
                    text: text.clone(),
                    image_path: None,
                    media_type: String::new(),
                },
                Content::Image {
                    path, media_type, ..
                } => artifact::CompactionContent {
                    kind: "Image".into(),
                    text: String::new(),
                    image_path: Some(path.into()),
                    media_type: media_type.clone(),
                },
            })
            .collect(),
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| artifact::CompactionToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect(),
        tool_call_id: message.tool_call_id.clone(),
    }
}
