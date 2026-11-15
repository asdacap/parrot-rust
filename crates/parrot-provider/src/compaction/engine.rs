use super::model::{CompactionGroup, CompactionPlan, CompactionResult, SummaryResponse};
use super::{CompactionEvent, CompactionProgress, Execution, scratch};
use parrot_core::{BoxFuture, CompactionFailure, Content, Error, Message, ProviderRequest, Role};

impl CompactionGroup {
    pub(super) fn can_spill(&self) -> bool {
        self.is_complete
            && self.messages.first().is_some_and(|message| {
                message.role == Role::Assistant && !message.tool_calls.is_empty()
            })
    }
}

pub(super) async fn compact(
    plan: Option<CompactionPlan>,
    execution: &Execution<'_>,
) -> Result<Option<CompactionResult>, Error> {
    let Some(plan) = plan else {
        return Ok(None);
    };
    CompactionRunner {
        plan: &plan,
        execution,
    }
    .run()
    .await
    .map(Some)
}

impl CompactionPlan {
    fn compose_summary_messages<'m>(
        &self,
        preceding_summary: &str,
        messages: impl IntoIterator<Item = &'m Message>,
    ) -> Vec<Message> {
        let mut request_messages = Vec::new();
        if !preceding_summary.is_empty() {
            request_messages.push(Message {
                role: Role::System,
                contents: vec![Content::Text(format!(
                    "{}{preceding_summary}",
                    self.summary_prefix
                ))],
                ..Message::default()
            });
        }
        request_messages.extend(messages.into_iter().cloned());
        request_messages.push(Message {
            role: Role::System,
            contents: vec![Content::Text(self.summary_instructions.clone())],
            ..Message::default()
        });
        request_messages
    }
}

struct CompactionRunner<'a> {
    plan: &'a CompactionPlan,
    execution: &'a Execution<'a>,
}

impl CompactionRunner<'_> {
    async fn run(&mut self) -> Result<CompactionResult, Error> {
        let plan = self.plan;
        let pending = &plan.groups[..plan.keep_group_from];
        let mut provider_groups = Vec::with_capacity(pending.len());
        let mut substituted = vec![false; pending.len()];
        for (index, group) in pending.iter().enumerate() {
            let messages = if self.execution.estimates.estimate_input_tokens(
                "",
                &[],
                &plan.compose_summary_messages("", &group.messages),
            )? > plan.input_budget
            {
                if !group.can_spill() {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                substituted[index] = true;
                let notice = scratch::spill_group(self.execution, &group.messages).await?;
                if self.execution.estimates.estimate_input_tokens(
                    "",
                    &[],
                    &plan.compose_summary_messages("", &notice),
                )? > plan.input_budget
                {
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
        for (index, mut messages) in provider_groups.into_iter().enumerate() {
            if !chunk.is_empty()
                && self.execution.estimates.estimate_input_tokens(
                    "",
                    &[],
                    &plan.compose_summary_messages(
                        &summary,
                        chunk.iter().flatten().chain(&messages),
                    ),
                )? > plan.input_budget
            {
                summary = self.fold_groups(&summary, &chunk, &mut progress).await?;
                chunk.clear();
            }
            if self.execution.estimates.estimate_input_tokens(
                "",
                &[],
                &plan.compose_summary_messages(&summary, chunk.iter().flatten().chain(&messages)),
            )? > plan.input_budget
            {
                if !pending[index].can_spill() || substituted[index] {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
                messages = scratch::spill_group(self.execution, &pending[index].messages).await?;
                substituted[index] = true;
                if self.execution.estimates.estimate_input_tokens(
                    "",
                    &[],
                    &plan.compose_summary_messages(
                        &summary,
                        chunk.iter().flatten().chain(&messages),
                    ),
                )? > plan.input_budget
                {
                    return Err(CompactionFailure::GroupExceedsBudget.into());
                }
            }
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
        let mut history = vec![summary.clone(), plan.fixed_message.clone()];
        history.extend(retained);
        if self.execution.estimates.estimate_input_tokens(
            &plan.instructions,
            &plan.tools,
            &history,
        )? > i64::from(plan.input_token_limit)
        {
            return Err(CompactionFailure::InputLimitExceeded.into());
        }
        Ok(CompactionResult {
            history,
            summary,
            watermark: plan.watermark,
        })
    }

    async fn report_progress(&self, progress: CompactionProgress) -> Result<(), Error> {
        self.check_cancellation()?;
        self.execution
            .effects
            .publish(
                CompactionEvent::Progress(progress),
                self.execution.cancellation,
            )
            .await
    }

    fn check_cancellation(&self) -> Result<(), Error> {
        if self.execution.cancellation.is_cancelled() {
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
            let messages = self
                .plan
                .compose_summary_messages(preceding_summary, pending.iter().flatten());
            let request = ProviderRequest {
                model: self.plan.model.clone(),
                messages,
                max_output_tokens: self.plan.summary_output_tokens,
                reasoning: self.plan.reasoning.clone(),
                ..ProviderRequest::default()
            };
            let outcome = self.execution.summarize(request).await?;
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
