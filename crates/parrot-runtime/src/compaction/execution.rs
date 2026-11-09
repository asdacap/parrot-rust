use super::{CompactionPlanner, CompactionSettings, ContextSize, PlanningError, PlanningInput};
use parrot_core::{
    CancellationToken, ProviderRequest,
    compaction::{
        self as core, CompactionEffects, CompactionFailure, CompactionPlan, CompactionProgress,
        SummaryProvider, SummaryResponse,
    },
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::{CompactionPhase, CompactionProgressEvent},
    values::config::ModelProfiles,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::{
    routing::{aliases::ResolvedModelSelection, registry::ProviderModel},
    sessions::{self, Content, Event, Message, ProviderSessions, Request, Role, Tool},
};
use parrot_storage::artifacts::compaction::{self as artifact, CompactionGroupBlobStore};
use std::{fmt, marker::PhantomData, ops::DerefMut, sync::Arc};

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
#[derive(Clone)]
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
        let plan = self.prepare_plan(selected, target, input)?;
        let estimates = CompactionEstimates::new(self, selected);
        self.run_plan(
            plan,
            estimates,
            CompactionResources {
                blobs,
                sessions,
                observer,
                cancellation,
            },
        )
        .await
    }

    pub(crate) fn prepare_plan(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
        input: &PlanningInput<'_>,
    ) -> Result<Option<CompactionPlan>, CompactionError> {
        let model = &selected.canonical_model;
        let target_budget = self.planner.resolve_policy(selected, target).target_tokens;
        Ok(self
            .planner
            .plan_retention(model, target_budget, input)?
            .map(|plan| CompactionPlan {
                groups: input
                    .groups
                    .iter()
                    .map(|group| core::CompactionGroup {
                        messages: group.messages.iter().cloned().map(Into::into).collect(),
                        is_complete: group.is_complete,
                    })
                    .collect(),
                keep_group_from: plan.keep_group_from,
                watermark: plan.watermark,
                input_budget: plan.input_budget,
                summary_output_tokens: plan.summary_output_tokens,
                input_token_limit: model.model.input_token_limit(),
                instructions: input.instructions.into(),
                tools: input.tools.iter().cloned().map(Into::into).collect(),
                fixed_message: input.fixed_message.clone().into(),
                summary_instructions: self.summary_instructions.clone(),
                summary_prefix: self.summary_prefix.clone(),
                model: model.model.id.clone(),
                reasoning: model.reasoning().map(Into::into),
            }))
    }

    pub(crate) fn prepare_input_plan(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
        input: &core::CompactionInput,
        tools: &[Tool],
    ) -> Result<Option<CompactionPlan>, CompactionError> {
        let messages: Vec<Vec<Message>> = input
            .groups
            .iter()
            .map(|group| group.messages.iter().cloned().map(Into::into).collect())
            .collect();
        let groups: Vec<super::CompactionGroup<'_>> = input
            .groups
            .iter()
            .zip(&messages)
            .map(|(group, messages)| super::CompactionGroup {
                messages,
                end_watermark: group.end_watermark,
                has_checkpoint_before: group.has_checkpoint_before,
                is_complete: group.is_complete,
            })
            .collect();
        let fixed: Message = input.fixed_message.clone().into();
        self.prepare_plan(
            selected,
            target,
            &PlanningInput {
                instructions: &input.instructions,
                tools,
                groups: &groups,
                base_watermark: input.base_watermark,
                fixed_message: &fixed,
            },
        )
    }

    pub(crate) async fn run_plan(
        &self,
        plan: Option<CompactionPlan>,
        estimates: CompactionEstimates,
        resources: CompactionResources<'_>,
    ) -> Result<Option<CompactionResult>, CompactionError> {
        let is_cancelled = || resources.cancellation.is_cancelled();
        let cancelled = || resources.cancellation.cancelled();
        let token = CancellationToken::new(&is_cancelled, &cancelled);
        let provider = SummaryProviderAdapter::<_, CompactionError>::new(
            estimates.selected.clone(),
            resources.sessions,
            BorrowedObserver(resources.observer),
        );
        let mut effects = CompactionEffectsAdapter::new(
            estimates,
            resources.blobs,
            BorrowedObserver(resources.observer),
        );
        core::compact(plan, &mut effects, &provider, token)
            .await
            .map(|result| result.map(Into::into))
    }
}

impl From<core::CompactionResult> for CompactionResult {
    fn from(result: core::CompactionResult) -> Self {
        Self {
            history: result.history.into_iter().map(Into::into).collect(),
            summary: result.summary.into(),
            retained_messages: result.retained_messages,
            watermark: result.watermark,
        }
    }
}

impl From<CompactionResult> for core::CompactionResult {
    fn from(result: CompactionResult) -> Self {
        Self {
            history: result.history.into_iter().map(Into::into).collect(),
            summary: result.summary.into(),
            retained_messages: result.retained_messages,
            watermark: result.watermark,
        }
    }
}

impl From<CompactionFailure> for CompactionError {
    fn from(failure: CompactionFailure) -> Self {
        match failure {
            CompactionFailure::Cancelled => Self::Cancelled,
            CompactionFailure::GroupExceedsBudget => {
                Self::Planning(PlanningError::GroupExceedsBudget)
            }
            CompactionFailure::TokenOverflow => Self::Planning(PlanningError::TokenOverflow),
            CompactionFailure::InputLimitExceeded => Self::InvalidOperation(failure.to_string()),
            CompactionFailure::NoSummary(reason) => Self::InvalidOperation(reason),
        }
    }
}

pub(crate) struct CompactionResources<'a> {
    pub blobs: &'a mut CompactionGroupBlobStore,
    pub sessions: &'a ProviderSessions,
    pub observer: &'a dyn CompactionObserver,
    pub cancellation: &'a dyn Cancellation,
}

pub(crate) struct CompactionEstimates {
    planner: Arc<CompactionPlanner>,
    selected: ProviderModel,
    templates: Arc<PromptTemplateCatalog>,
}

impl CompactionEstimates {
    pub(crate) fn new(compactor: &Compactor, selected: &ResolvedModelSelection) -> Self {
        Self {
            planner: compactor.planner.clone(),
            selected: selected.canonical_model.clone(),
            templates: compactor.templates.clone(),
        }
    }

    pub(crate) fn estimate_tokens(
        &self,
        messages: &[parrot_core::Message],
    ) -> Result<i64, CompactionError> {
        let messages: Vec<Message> = messages.iter().cloned().map(Into::into).collect();
        CompactionPlanner::estimate_tokens(&self.selected, &messages).map_err(CompactionError::from)
    }

    pub(crate) fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[parrot_core::Message],
    ) -> Result<i64, CompactionError> {
        let messages: Vec<Message> = next_group.iter().cloned().map(Into::into).collect();
        self.planner
            .estimate_summary_request_tokens(
                &self.selected,
                preceding_summary,
                chunk_tokens,
                &messages,
            )
            .map_err(CompactionError::from)
    }

    pub(crate) fn estimate_context_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, CompactionError> {
        let messages: Vec<Message> = messages.iter().cloned().map(Into::into).collect();
        let tools: Vec<Tool> = definitions.iter().cloned().map(Into::into).collect();
        CompactionPlanner::estimate_input_tokens(&self.selected, instructions, &tools, &messages)
            .map_err(CompactionError::from)
    }

    pub(crate) fn spill_group(
        &self,
        blobs: &mut CompactionGroupBlobStore,
        messages: &[parrot_core::Message],
        cancellation: CancellationToken<'_>,
    ) -> Result<Vec<parrot_core::Message>, CompactionError> {
        let group = artifact::CompactionGroup {
            messages: messages
                .iter()
                .cloned()
                .map(Message::from)
                .map(|message| artifact_message(&message))
                .collect(),
        };
        let path = blobs.persist(&group, &CoreCancellation(cancellation))?;
        let notice = self
            .templates
            .render(
                "compaction.oversized-tool-group-notice",
                &[("path".into(), path.to_string_lossy().as_ref().into())],
                &Uncancelled,
            )
            .map_err(|error| CompactionError::Template(error.to_string()))?;
        Ok(vec![Message::system(notice).into()])
    }
}

pub(crate) struct CompactionEffectsAdapter<B, O, E> {
    estimates: CompactionEstimates,
    blobs: B,
    observer: O,
    error: PhantomData<fn() -> E>,
}

impl<B, O, E> CompactionEffectsAdapter<B, O, E> {
    pub(crate) fn new(estimates: CompactionEstimates, blobs: B, observer: O) -> Self {
        Self {
            estimates,
            blobs,
            observer,
            error: PhantomData,
        }
    }
}

impl<B, O, E> CompactionEffects for CompactionEffectsAdapter<B, O, E>
where
    B: DerefMut<Target = CompactionGroupBlobStore> + Send + Sync,
    O: CompactionObserver,
    E: From<CompactionError> + From<CompactionFailure> + Send,
{
    type Error = E;

    fn estimate_tokens(&self, messages: &[parrot_core::Message]) -> Result<i64, E> {
        self.estimates.estimate_tokens(messages).map_err(Into::into)
    }

    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[parrot_core::Message],
    ) -> Result<i64, E> {
        self.estimates
            .estimate_summary_tokens(preceding_summary, chunk_tokens, next_group)
            .map_err(Into::into)
    }

    fn estimate_context_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, E> {
        self.estimates
            .estimate_context_tokens(instructions, definitions, messages)
            .map_err(Into::into)
    }

    fn spill_group<'a, 'c>(
        &'a mut self,
        messages: &'a [parrot_core::Message],
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::Message>, E>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.estimates
                .spill_group(&mut self.blobs, messages, cancellation)
                .map_err(Into::into)
        })
    }

    fn report_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), E>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.observer
                .observe_progress(
                    CompactionProgressEvent {
                        phase: CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                        completed_groups: progress.completed_groups as u64,
                        total_groups: progress.total_groups as u64,
                        ..CompactionProgressEvent::new()
                    },
                    &CoreCancellation(cancellation),
                )
                .await
                .map_err(|error| CompactionError::from(error).into())
        })
    }
}

pub(crate) struct SummaryProviderAdapter<'a, O, E> {
    selected: ProviderModel,
    sessions: &'a ProviderSessions,
    observer: O,
    error: PhantomData<fn() -> E>,
}

impl<'a, O, E> SummaryProviderAdapter<'a, O, E> {
    pub(crate) fn new(
        selected: ProviderModel,
        sessions: &'a ProviderSessions,
        observer: O,
    ) -> Self {
        Self {
            selected,
            sessions,
            observer,
            error: PhantomData,
        }
    }
}

impl<O: CompactionObserver, E: From<CompactionError> + Send> SummaryProvider
    for SummaryProviderAdapter<'_, O, E>
{
    type Error = E;

    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, E>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.collect(request.into(), CoreCancellation(cancellation))
                .await
                .map_err(Into::into)
        })
    }
}

impl<O: CompactionObserver, E> SummaryProviderAdapter<'_, O, E> {
    async fn collect(
        &self,
        request: Request,
        cancellation: CoreCancellation<'_>,
    ) -> Result<SummaryResponse, CompactionError> {
        cancellation
            .check()
            .map_err(|_| CompactionError::Cancelled)?;
        let session = self.sessions.get(self.selected.provider.clone()).await?;
        let mut stream = session.call(request)?;
        let result = async {
            let mut summary = None;
            let mut completion_details = None;
            let mut reasoning_bytes = 0;
            while let Some(event) = stream.next(&cancellation).await? {
                if summary.is_some() {
                    return Err(CompactionError::InvalidOperation(
                        "The compaction provider emitted an event after completion.".into(),
                    ));
                }
                match event.base_event() {
                    Event::Retry { .. } => self.observer.observe(&event, &cancellation).await?,
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
                Some(summary) => SummaryResponse::Summary(summary),
                None => SummaryResponse::NoSummary(format!(
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

struct CoreCancellation<'a>(CancellationToken<'a>);
impl Cancellation for CoreCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        self.0.cancelled()
    }
}

struct BorrowedObserver<'a>(&'a dyn CompactionObserver);
impl CompactionObserver for BorrowedObserver<'_> {
    fn observe<'a>(
        &'a self,
        event: &'a Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>> {
        self.0.observe(event, cancellation)
    }
    fn observe_progress<'a>(
        &'a self,
        progress: CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), sessions::Error>> {
        self.0.observe_progress(progress, cancellation)
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
