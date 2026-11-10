pub use crate::CompactionFailure;
use crate::{
    BoxFuture, CancellationToken, Content, Error, ErrorCode, ErrorDetail, Message, Provider,
    ProviderRequest, Role, States,
};
pub use crate::{
    CompactionGroup, CompactionHistoryGroup, CompactionInput, CompactionPlan, CompactionProgress,
    CompactionResult, ConversationHistoryItem, ConversationState, ConversationSummary,
    FittedCompaction, SummaryResponse,
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

impl CompactionGroup {
    pub fn can_spill(&self) -> bool {
        self.is_complete
            && self.messages.first().is_some_and(|message| {
                message.role == Role::Assistant && !message.tool_calls.is_empty()
            })
    }
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
        let message = failure.to_string();
        let code = if failure == CompactionFailure::TokenOverflow {
            ErrorCode::Unexpected
        } else {
            ErrorCode::InvalidOperation
        };
        Self {
            code,
            detail: Some(ErrorDetail::Compaction(failure)),
            ..Self::failed(message)
        }
    }
}

/// Executes one safe-prefix plan, retaining atomic groups and joining every provider call.
/// Native callers retain lifecycle events and persistence. Core also exposes planning and fitting.
pub async fn compact(
    plan: Option<CompactionPlan>,
    states: &mut dyn States,
    provider: &dyn Provider,
    cancellation: CancellationToken<'_>,
) -> Result<Option<CompactionResult>, Error> {
    let Some(plan) = plan else {
        return Ok(None);
    };
    CompactionRunner {
        plan: &plan,
        states,
        provider,
        cancellation,
    }
    .run()
    .await
    .map(Some)
}

struct CompactionRunner<'a, 'c> {
    plan: &'a CompactionPlan,
    states: &'a mut dyn States,
    provider: &'a dyn Provider,
    cancellation: CancellationToken<'c>,
}

impl CompactionRunner<'_, '_> {
    async fn run(&mut self) -> Result<CompactionResult, Error> {
        let plan = self.plan;
        let pending = &plan.groups[..plan.keep_group_from];
        let mut provider_groups = Vec::with_capacity(pending.len());
        let mut substituted = vec![false; pending.len()];
        for (index, group) in pending.iter().enumerate() {
            let messages = if self
                .provider
                .estimate_summary_tokens("", 0, &group.messages)?
                > plan.input_budget
            {
                if !group.can_spill() {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                substituted[index] = true;
                let notice = self
                    .states
                    .spill_group(&group.messages, self.cancellation)
                    .await?;
                if self.provider.estimate_summary_tokens("", 0, &notice)? > plan.input_budget {
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
                    .provider
                    .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                    > plan.input_budget
            {
                summary = self.fold_groups(&summary, &chunk, &mut progress).await?;
                chunk.clear();
                chunk_tokens = 0;
            }
            if self
                .provider
                .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                > plan.input_budget
            {
                if !pending[index].can_spill() || substituted[index] {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                messages = self
                    .states
                    .spill_group(&pending[index].messages, self.cancellation)
                    .await?;
                substituted[index] = true;
                if self
                    .provider
                    .estimate_summary_tokens(&summary, chunk_tokens, &messages)?
                    > plan.input_budget
                {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
            }
            chunk_tokens = chunk_tokens
                .checked_add(self.provider.estimate_tokens(&messages)?)
                .ok_or_else(|| Error::from(CompactionFailure::TokenOverflow))?;
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
            .provider
            .estimate_input_tokens(&plan.instructions, &plan.tools, &history)?
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

    async fn report_progress(&self, progress: CompactionProgress) -> Result<(), Error> {
        self.check_cancellation()?;
        self.states
            .report_compaction_progress(progress, self.cancellation)
            .await
    }

    fn check_cancellation(&self) -> Result<(), Error> {
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
    ) -> BoxFuture<'a, Result<String, Error>> {
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
