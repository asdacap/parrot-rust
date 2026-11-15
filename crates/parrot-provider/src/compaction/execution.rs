use super::{CompactionEffects, CompactionEvent, model::SummaryResponse};
use crate::sessions::{Event, ProviderSessions};
use parrot_core::{Error, ErrorCode, NeverCancelled, ProviderRequest};
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub(super) struct Execution<'a> {
    pub estimates: &'a dyn parrot_core::Provider,
    pub sessions: &'a ProviderSessions,
    pub selected: &'a crate::routing::registry::ProviderModel,
    pub effects: &'a dyn CompactionEffects,
    pub templates: &'a PromptTemplateCatalog,
    pub cancellation: &'a dyn Cancellation,
}

impl Execution<'_> {
    pub async fn summarize(&self, request: ProviderRequest) -> Result<SummaryResponse, Error> {
        self.cancellation.check().map_err(|_| Error::cancelled())?;
        let session = self
            .sessions
            .get(self.selected.provider.clone())
            .await
            .map_err(provider_error)?;
        let mut stream = session.call(request.into()).map_err(provider_error)?;
        let result = async {
            let mut summary = SummaryText::default();
            while let Some(event) = stream
                .next(self.cancellation)
                .await
                .map_err(provider_error)?
            {
                summary.observe(&event)?;
                if matches!(event.base_event(), Event::Retry { .. }) {
                    self.effects
                        .publish(CompactionEvent::Retry(event), self.cancellation)
                        .await?;
                }
            }
            Ok(summary.finish())
        }
        .await;
        stream.close().await.map_err(provider_error)?;
        result
    }
}

fn provider_error(error: crate::sessions::Error) -> Error {
    let code = crate::sessions::diagnostic_session::failure_code(&error);
    if code == ErrorCode::Cancelled {
        return Error::cancelled();
    }
    Error {
        code,
        response_body: error.response_body().to_owned(),
        ..Error::failed(error.to_string())
    }
}

#[derive(Default)]
struct SummaryText {
    summary: Option<String>,
    completion_details: Option<String>,
    reasoning_bytes: usize,
}

impl SummaryText {
    fn observe(&mut self, event: &Event) -> Result<(), Error> {
        if self.summary.is_some() {
            return Err(Error {
                code: ErrorCode::InvalidOperation,
                ..Error::failed("The compaction provider emitted an event after completion.")
            });
        }
        match event.base_event() {
            Event::Completed(completion) if !completion.assistant_text.is_empty() => {
                self.summary = Some(completion.assistant_text.clone());
            }
            Event::Completed(completion) => {
                self.completion_details = Some(format!(
                    "The compaction provider completed with empty assistant text (finish reason: {:?}; assistant text length: {} UTF-8 bytes; provider input tokens: {}; provider output tokens: {}).",
                    completion.finish_reason,
                    completion.assistant_text.len(),
                    completion.input_tokens,
                    completion.output_tokens,
                ));
            }
            Event::ReasoningDelta { text, .. } => self.reasoning_bytes += text.len(),
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> SummaryResponse {
        match self.summary {
            Some(summary) => SummaryResponse::Summary(summary),
            None => SummaryResponse::NoSummary(format!(
                "{} Reasoning delta length: {} UTF-8 bytes.",
                self.completion_details.unwrap_or_else(|| {
                    "The compaction provider stream ended without a Completed event.".into()
                }),
                self.reasoning_bytes,
            )),
        }
    }
}

pub(super) async fn publish_terminal(
    effects: &dyn CompactionEffects,
    event: CompactionEvent,
) -> Result<(), Error> {
    effects.publish(event, &NeverCancelled).await
}
