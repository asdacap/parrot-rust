use parrot_core::{
    BoxFuture, CancellationToken, Content, Error, Message, ProviderRequest, Reasoning, Role,
    compaction::{CompactionGroup, CompactionPlan, CompactionProgress, SummaryResponse},
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
        instructions: String::new(),
        tools: Vec::new(),
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
    pub scratch_writes: Mutex<Vec<Vec<u8>>>,
    pub contexts: Arc<Mutex<Vec<ProviderRequest>>>,
    pub context_failure: Option<(usize, Error)>,
}

impl Effects {
    pub fn estimate_tokens(&self, messages: &[Message]) -> Result<i64, Error> {
        Ok(messages
            .iter()
            .map(|message| {
                message
                    .contents
                    .iter()
                    .map(|content| match content {
                        Content::Text(text) => text.len() as i64,
                        Content::Image { .. } | Content::Artifact(_) => 10,
                    })
                    .sum::<i64>()
            })
            .sum())
    }

    pub fn estimate_summary_tokens(
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

    pub fn estimate_context_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        let mut contexts = self.contexts.lock().unwrap();
        contexts.push(ProviderRequest {
            instructions: instructions.into(),
            tools: definitions.to_vec(),
            messages: messages.to_vec(),
            ..ProviderRequest::default()
        });
        if let Some((call, error)) = &self.context_failure {
            if *call == contexts.len() {
                return Err(error.clone());
            }
        }
        self.estimate_tokens(messages)
    }

    pub fn write_to_scratch<'a, 'c>(
        &'a self,
        contents: &'a [u8],
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.scratch_writes.lock().unwrap().push(contents.to_vec());
            Ok("blob".into())
        })
    }

    pub fn report_progress<'a, 'c>(
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
    estimates: Arc<Effects>,
}

impl Provider {
    pub fn with_estimates(&self, effects: &Effects) -> Self {
        Self {
            estimates: Arc::new(Effects {
                contexts: effects.contexts.clone(),
                context_failure: effects.context_failure.clone(),
                ..Effects::default()
            }),
            ..self.clone()
        }
    }

    pub fn new(replies: impl IntoIterator<Item = SummaryResponse>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().map(Ok).collect())),
            ..Self::default()
        }
    }
}

impl parrot_core::Provider for Provider {
    fn estimate_tokens(&self, messages: &[Message]) -> Result<i64, Error> {
        self.estimates.estimate_tokens(messages)
    }
    fn estimate_summary_tokens(
        &self,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[Message],
    ) -> Result<i64, Error> {
        self.estimates
            .estimate_summary_tokens(preceding_summary, chunk_tokens, next_group)
    }
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        self.estimates
            .estimate_context_tokens(instructions, definitions, messages)
    }

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

impl parrot_core::States for Effects {
    fn get_config(
        &self,
        key: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, Error>> {
        Box::pin(async move {
            match key {
                parrot_core::ConfigKey::OversizedToolGroupTemplate => {
                    Ok(parrot_core::ConfigValue::PromptTemplate(Arc::new(
                        parrot_core::PromptTemplate::compile(
                            "test.compaction-notice",
                            "{path}",
                            "scalar",
                            &["path".into()],
                            &["path".into()],
                        )?,
                    )))
                }
                _ => Err(Error::unsupported("test compaction configuration")),
            }
        })
    }
    fn write_to_scratch<'a, 'c>(
        &'a self,
        contents: &'a [u8],
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        self.write_to_scratch(contents, cancellation)
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        self.report_progress(progress, cancellation)
    }
}
