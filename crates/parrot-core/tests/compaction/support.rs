use parrot_core::{
    BoxFuture, CancellationToken, Content, Error, Message, ProviderRequest, Reasoning, Role,
    compaction::{
        CompactionEffects, CompactionGroup, CompactionPlan, CompactionProgress, SummaryProvider,
        SummaryResponse,
    },
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

pub fn message(text: &str) -> Message {
    Message {
        role: Role::User,
        contents: vec![Content::Text(text.into())],
        ..Message::default()
    }
}

pub fn plan(groups: &[&str]) -> CompactionPlan {
    CompactionPlan {
        groups: groups
            .iter()
            .map(|text| CompactionGroup {
                messages: vec![message(text)],
                is_complete: true,
            })
            .collect(),
        keep_group_from: groups.len() - 1,
        watermark: 42,
        input_budget: 1000,
        summary_output_tokens: 19,
        input_token_limit: 10000,
        fixed_message: message("status"),
        summary_instructions: "instructions".into(),
        summary_prefix: "prefix:".into(),
        model: "summary-model".into(),
        reasoning: Some(Reasoning {
            effort: "low".into(),
            summary: "auto".into(),
        }),
    }
}

#[derive(Default)]
pub struct Effects {
    pub progress: Mutex<Vec<CompactionProgress>>,
    pub spills: Vec<Vec<Message>>,
}

impl CompactionEffects for Effects {
    type Error = Error;

    fn estimate_tokens(&self, messages: &[Message]) -> Result<i64, Error> {
        Ok(messages
            .iter()
            .map(|message| {
                message
                    .contents
                    .iter()
                    .map(|content| match content {
                        Content::Text(text) => text.len() as i64,
                        Content::Image { .. } => 10,
                    })
                    .sum::<i64>()
            })
            .sum())
    }

    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[Message],
    ) -> Result<i64, Error> {
        Ok(if preceding_summary.is_empty() {
            0
        } else {
            preceding_summary.len() as i64 + 7
        } + chunk_tokens
            + self.estimate_tokens(next_group)?)
    }

    fn estimate_context_tokens(&self, messages: &[Message]) -> Result<i64, Error> {
        self.estimate_tokens(messages)
    }

    fn spill_group(
        &mut self,
        messages: &[Message],
        _: CancellationToken<'_>,
    ) -> Result<Vec<Message>, Error> {
        self.spills.push(messages.to_vec());
        Ok(vec![message("blob")])
    }

    fn report_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.progress.lock().unwrap().push(progress);
            Ok(())
        })
    }
}

#[derive(Clone, Default)]
pub struct Provider {
    pub replies: Arc<Mutex<VecDeque<Result<SummaryResponse, Error>>>>,
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl Provider {
    pub fn new(replies: impl IntoIterator<Item = SummaryResponse>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().map(Ok).collect())),
            ..Self::default()
        }
    }
}

impl SummaryProvider for Provider {
    type Error = Error;

    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<SummaryResponse, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected summary request")
        })
    }
}
