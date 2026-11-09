use crate::{
    BoxFuture, CancellationToken, Content, ContextSnapshot, Error, ErrorCode, Message,
    ProviderRequest, Reasoning, Role, ToolDefinition,
};
use std::fmt;

mod fitting;
mod planning;
mod preparation;
pub use fitting::{compact_conversation, fit_compaction, fit_status};
pub use planning::{
    CompactionConfiguration, CompactionPolicy, PlanningError, group_messages, is_complete,
};
pub use preparation::prepare_compaction;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionGroup {
    pub messages: Vec<Message>,
    pub is_complete: bool,
}

impl CompactionGroup {
    pub fn can_spill(&self) -> bool {
        self.is_complete
            && self.messages.first().is_some_and(|message| {
                message.role == Role::Assistant && !message.tool_calls.is_empty()
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionHistoryGroup {
    pub messages: Vec<Message>,
    pub end_watermark: i64,
    pub has_checkpoint_before: bool,
    pub is_complete: bool,
}

/// Captures durable history facts and the independent current provider-visible view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversationState {
    pub summary: Option<ConversationSummary>,
    /// Contains the chronological durable tail after the captured summary watermark.
    pub history: Vec<ConversationHistoryItem>,
    pub checkpoint_sequences: Vec<i64>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationSummary {
    pub content: String,
    pub watermark: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationHistoryItem {
    pub sequence: i64,
    pub tool_owner_sequence: Option<i64>,
    pub is_compaction_status: bool,
    pub message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionInput {
    pub groups: Vec<CompactionHistoryGroup>,
    pub base_watermark: i64,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub fixed_message: Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionPlan {
    pub groups: Vec<CompactionGroup>,
    pub keep_group_from: usize,
    pub watermark: i64,
    pub input_budget: i64,
    pub summary_output_tokens: i32,
    pub input_token_limit: i32,
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
    pub fixed_message: Message,
    pub summary_instructions: String,
    pub summary_prefix: String,
    pub model: String,
    pub reasoning: Option<Reasoning>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    pub history: Vec<Message>,
    pub summary: Message,
    pub retained_messages: usize,
    pub watermark: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FittedCompaction {
    pub result: CompactionResult,
    pub status: String,
}

/// Renders native status for exactly the context supplied by core.
pub trait StatusRenderer: Send + Sync {
    type Error: Send;

    /// Observes and renders status without estimating, fitting, or persisting context.
    fn render<'a, 'c>(
        &'a self,
        context: ContextSnapshot,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Self::Error>>
    where
        'c: 'a;
}

/// Performs prompt and lifecycle effects without assembling compaction input.
pub trait CompactionPreparationEffects: Send + Sync {
    type Error: Send;

    /// Publishes the start before preparation, independent of request cancellation.
    fn report_started(&self) -> BoxFuture<'_, Result<(), Self::Error>>;
    fn sample_epoch<'a, 'c>(
        &'a self,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Self::Error>>
    where
        'c: 'a;
    fn render_instructions<'a, 'c>(
        &'a self,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Self::Error>>
    where
        'c: 'a;
    /// Publishes a preparation failure, independent of request cancellation.
    fn report_failed(&self, message: String) -> BoxFuture<'_, Result<(), Self::Error>>;
}

pub struct CompactionDependencies<'a> {
    pub configuration: &'a CompactionConfiguration,
    pub preparation: &'a dyn CompactionPreparationEffects<Error = Error>,
    pub effects: &'a mut dyn CompactionEffects<Error = Error>,
    pub provider: &'a dyn SummaryProvider<Error = Error>,
    pub status: &'a dyn StatusRenderer<Error = Error>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionProgress {
    pub completed_groups: usize,
    pub total_groups: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryResponse {
    Summary(String),
    NoSummary(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionFailure {
    Cancelled,
    GroupExceedsBudget,
    TokenOverflow,
    InputLimitExceeded,
    FittedInputLimitExceeded,
    NoSafeBoundary,
    NoSummary(String),
}

impl fmt::Display for CompactionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "The operation was canceled.",
            Self::GroupExceedsBudget => {
                "A complete conversation group exceeds the compaction input budget."
            }
            Self::TokenOverflow => "Token estimate overflow.",
            Self::InputLimitExceeded | Self::FittedInputLimitExceeded => {
                "The compacted conversation exceeds the selected model input limit."
            }
            Self::NoSafeBoundary => {
                "The conversation has no safe compaction boundary before the input limit."
            }
            Self::NoSummary(reason) => reason,
        })
    }
}

impl std::error::Error for CompactionFailure {}

impl From<CompactionFailure> for Error {
    fn from(failure: CompactionFailure) -> Self {
        if failure == CompactionFailure::Cancelled {
            return Self::cancelled();
        }
        Self {
            code: if failure == CompactionFailure::TokenOverflow {
                ErrorCode::Unexpected
            } else {
                ErrorCode::InvalidOperation
            },
            ..Self::failed(failure.to_string())
        }
    }
}

/// Performs model-aware estimates, blob publication, and awaited summary progress.
/// Native error types are retained while request, message, and result payloads stay core-owned.
pub trait CompactionEffects: Send + Sync {
    type Error: From<CompactionFailure> + Send;

    fn estimate_tokens(&self, messages: &[Message]) -> Result<i64, Self::Error>;
    /// Estimates the actual preceding summary, accumulated chunk, and next group.
    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[Message],
    ) -> Result<i64, Self::Error>;
    /// Estimates the complete replacement conversation, including instructions and tools.
    fn estimate_context_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Self::Error>;
    /// Publishes the original group and returns notices containing the actual saved path.
    fn spill_group<'a, 'c>(
        &'a mut self,
        messages: &'a [Message],
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<Message>, Self::Error>>
    where
        'c: 'a;
    /// Publishes aggregate progress without disclosing summary content.
    fn report_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Self::Error>>
    where
        'c: 'a;
}

/// Collects private summary streams, preserving empty/missing-completion diagnostics.
pub trait SummaryProvider: Send + Sync {
    type Error: Send;

    /// Invokes exactly the supplied request, awaits retry publication and stream cleanup,
    /// and never publishes assistant or reasoning content as conversation output.
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, Self::Error>>
    where
        'c: 'a;
}

/// Executes one safe-prefix plan, retaining atomic groups and joining every provider call.
/// Native callers retain lifecycle events and persistence. Core also exposes planning and fitting.
pub async fn compact<E: From<CompactionFailure> + Send>(
    plan: Option<CompactionPlan>,
    effects: &mut dyn CompactionEffects<Error = E>,
    provider: &dyn SummaryProvider<Error = E>,
    cancellation: CancellationToken<'_>,
) -> Result<Option<CompactionResult>, E> {
    let Some(plan) = plan else {
        return Ok(None);
    };
    CompactionRunner {
        plan: &plan,
        effects,
        provider,
        cancellation,
    }
    .run()
    .await
    .map(Some)
}

struct CompactionRunner<'a, 'c, E> {
    plan: &'a CompactionPlan,
    effects: &'a mut dyn CompactionEffects<Error = E>,
    provider: &'a dyn SummaryProvider<Error = E>,
    cancellation: CancellationToken<'c>,
}

impl<E: From<CompactionFailure> + Send> CompactionRunner<'_, '_, E> {
    async fn run(&mut self) -> Result<CompactionResult, E> {
        let plan = self.plan;
        let pending = &plan.groups[..plan.keep_group_from];
        let mut provider_groups = Vec::with_capacity(pending.len());
        let mut substituted = vec![false; pending.len()];
        for (index, group) in pending.iter().enumerate() {
            let messages = if self
                .effects
                .estimate_summary_tokens("", 0, &group.messages)?
                > plan.input_budget
            {
                if !group.can_spill() {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                substituted[index] = true;
                let notice = self
                    .effects
                    .spill_group(&group.messages, self.cancellation)
                    .await?;
                if self.effects.estimate_summary_tokens("", 0, &notice)? > plan.input_budget {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                notice
            } else {
                group.messages.clone()
            };
            provider_groups.push(messages);
        }
        let mut progress = CompactionProgress::default();
        let mut summary = String::new();
        let mut chunk = Vec::new();
        let mut chunk_tokens = 0;
        for (index, mut messages) in provider_groups.into_iter().enumerate() {
            if !chunk.is_empty()
                && self
                    .effects
                    .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                    > plan.input_budget
            {
                summary = self.fold_groups(&summary, &chunk, &mut progress).await?;
                chunk.clear();
                chunk_tokens = 0;
            }
            if self
                .effects
                .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                > plan.input_budget
            {
                if !pending[index].can_spill() || substituted[index] {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                messages = self
                    .effects
                    .spill_group(&pending[index].messages, self.cancellation)
                    .await?;
                substituted[index] = true;
                if self
                    .effects
                    .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                    > plan.input_budget
                {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
            }
            chunk_tokens = chunk_tokens
                .checked_add(self.effects.estimate_tokens(&messages)?)
                .ok_or_else(|| E::from(CompactionFailure::TokenOverflow))?;
            chunk.push(messages);
        }
        if !chunk.is_empty() {
            summary = self.fold_groups(&summary, &chunk, &mut progress).await?;
        }
        let summary = Message {
            role: Role::System,
            contents: vec![Content::Text(format!("{}{summary}", plan.summary_prefix))],
            ..Message::default()
        };
        let retained: Vec<_> = plan.groups[plan.keep_group_from..]
            .iter()
            .flat_map(|group| group.messages.iter().cloned())
            .collect();
        let retained_messages = retained.len();
        let mut history = vec![summary.clone(), plan.fixed_message.clone()];
        history.extend(retained);
        if self
            .effects
            .estimate_context_tokens(&plan.instructions, &plan.tools, &history)?
            > i64::from(plan.input_token_limit)
        {
            return Err(CompactionFailure::InputLimitExceeded.into());
        }
        Ok(CompactionResult {
            history,
            summary,
            retained_messages,
            watermark: plan.watermark,
        })
    }

    async fn report_progress(&self, progress: CompactionProgress) -> Result<(), E> {
        self.check_cancellation()?;
        self.effects
            .report_progress(progress, self.cancellation)
            .await
    }

    fn check_cancellation(&self) -> Result<(), E> {
        if self.cancellation.is_cancelled() {
            Err(CompactionFailure::Cancelled.into())
        } else {
            Ok(())
        }
    }

    fn fold_groups<'a>(
        &'a mut self,
        preceding_summary: &'a str,
        pending: &'a [Vec<Message>],
        progress: &'a mut CompactionProgress,
    ) -> BoxFuture<'a, Result<String, E>> {
        Box::pin(async move {
            progress.total_groups += 1;
            self.report_progress(*progress).await?;
            self.check_cancellation()?;
            let mut messages = vec![Message {
                role: Role::System,
                contents: vec![Content::Text(self.plan.summary_instructions.clone())],
                ..Message::default()
            }];
            if !preceding_summary.is_empty() {
                messages.push(Message {
                    role: Role::System,
                    contents: vec![Content::Text(format!(
                        "{}{preceding_summary}",
                        self.plan.summary_prefix
                    ))],
                    ..Message::default()
                });
            }
            messages.extend(pending.iter().flatten().cloned());
            let request = ProviderRequest {
                model: self.plan.model.clone(),
                messages,
                max_output_tokens: self.plan.summary_output_tokens,
                reasoning: self.plan.reasoning.clone(),
                ..ProviderRequest::default()
            };
            let outcome = self.provider.summarize(request, self.cancellation).await?;
            progress.completed_groups += 1;
            self.report_progress(*progress).await?;
            match outcome {
                SummaryResponse::Summary(summary) => return Ok(summary),
                SummaryResponse::NoSummary(reason) if pending.len() == 1 => {
                    return Err(CompactionFailure::NoSummary(reason).into());
                }
                SummaryResponse::NoSummary(_) => {}
            }
            let half = pending.len().div_ceil(2);
            let first = self
                .fold_groups(preceding_summary, &pending[..half], progress)
                .await?;
            self.fold_groups(&first, &pending[half..], progress).await
        })
    }
}
