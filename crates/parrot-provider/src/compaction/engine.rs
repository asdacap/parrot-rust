use super::model::{CompactionPlan, CompactionResult, SummaryResponse};
use super::{CompactionEvent, CompactionProgress, Execution};
use crate::sessions::Request;
use parrot_core::{BoxFuture, CompactionFailure, Content, Error, Message, Role};

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
        let provider_groups = pending
            .iter()
            .map(|group| self.fit_group("", &group.messages))
            .collect::<Result<Vec<_>, _>>()?;
        let mut progress = CompactionProgress::default();
        let mut summary = String::new();
        let mut chunk = Vec::new();
        for mut messages in provider_groups {
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
                messages = self.fit_group(&summary, &messages)?;
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

    fn fit_group(&self, summary: &str, messages: &[Message]) -> Result<Vec<Message>, Error> {
        self.check_cancellation()?;
        let fits = |messages: &[Message]| {
            self.execution
                .estimates
                .estimate_input_tokens(
                    "",
                    &[],
                    &self.plan.compose_summary_messages(summary, messages),
                )
                .map(|tokens| tokens <= self.plan.input_budget)
        };
        if fits(messages)? {
            return Ok(messages.to_vec());
        }
        let notice = self
            .execution
            .templates
            .render(
                "compaction.truncated-group-notice",
                &[],
                self.execution.cancellation,
            )
            .map_err(|error| Error::template(error.to_string()))?;
        let mut limit = messages
            .iter()
            .flat_map(|message| &message.contents)
            .filter_map(|content| match content {
                Content::Text(text) => Some(text.len()),
                Content::Image { .. } => None,
            })
            .max()
            .unwrap_or(0);
        loop {
            self.check_cancellation()?;
            let mut clipped = messages.to_vec();
            for message in &mut clipped {
                message.provider_id.clear();
                message.provider_content.clear();
                for content in &mut message.contents {
                    match content {
                        Content::Text(text) if text.len() > limit => {
                            let mut end = limit;
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                            text.push_str(&notice);
                        }
                        Content::Image { .. } => *content = Content::Text(notice.clone()),
                        _ => {}
                    }
                }
            }
            if fits(&clipped)? {
                return Ok(clipped);
            }
            if limit == 0 {
                return Err(CompactionFailure::GroupExceedsBudget.into());
            }
            limit /= 2;
        }
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
            self.check_cancellation()?;
            let mut messages = self
                .plan
                .compose_summary_messages(preceding_summary, pending.iter().flatten());
            if self
                .execution
                .estimates
                .estimate_input_tokens("", &[], &messages)?
                > self.plan.input_budget
            {
                if pending.len() > 1 {
                    let half = pending.len().div_ceil(2);
                    let first = self
                        .fold_groups(preceding_summary, &pending[..half], progress)
                        .await?;
                    return self.fold_groups(&first, &pending[half..], progress).await;
                }
                let clipped = self.fit_group(preceding_summary, &pending[0])?;
                messages = self
                    .plan
                    .compose_summary_messages(preceding_summary, &clipped);
            }
            progress.total_groups += 1;
            self.report_progress(*progress).await?;
            let request = Request {
                fast_mode: false,
                model: self.plan.model.clone(),
                instructions: String::new(),
                messages: messages.into_iter().map(Into::into).collect(),
                tools: Vec::new(),
                max_output_tokens: self.plan.summary_output_tokens,
                reasoning: self.plan.reasoning.clone(),
                provider_preferences: String::new(),
                include_router_metadata: false,
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
