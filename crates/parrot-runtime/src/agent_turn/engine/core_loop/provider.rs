use super::*;
use crate::agent_turn::provider_call::{
    OutputWriter, ProviderActivity, ProviderCallError, ProviderUsage, StreamedFragmentBuffer,
    translate_provider_event,
};
use parrot_provider::{
    routing::aliases::ResolvedModelSelection,
    sessions::{Completion, EventStream, Request, Session},
};
use parrot_storage::conversation::ConversationOrigin;

pub(super) struct RuntimeProvider<'a> {
    pub(super) model: ResolvedModelSelection,
    pub(super) sessions: &'a ProviderSessions,
    pub(super) history: &'a AgentHistoryView,
    pub(super) publisher: &'a Publisher,
    pub(super) event_id: &'a (dyn Fn() -> String + Send + Sync),
    pub(super) output: &'a dyn SessionOutput,
    pub(super) activity: &'a dyn SessionActivity,
    pub(super) state: &'a Mutex<State>,
    pub(super) events: ContextEmitter<'a>,
}

#[derive(Clone, Copy)]
struct StreamEffects<'a> {
    history: &'a AgentHistoryView,
    publisher: &'a Publisher,
    event_id: &'a (dyn Fn() -> String + Send + Sync),
    output: &'a dyn OutputWriter,
    activity: &'a dyn ProviderActivity,
}

#[derive(Default)]
struct StreamProgress {
    fragments: StreamedFragmentBuffer,
    attempt: u32,
    streamed_text: bool,
}

impl parrot_core::Provider for RuntimeProvider<'_> {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, CoreError> {
        crate::compaction::CompactionEstimates::new(&self.model)
            .estimate_input_tokens(instructions, definitions, messages)
            .map_err(Into::into)
    }
    fn summarize<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<parrot_core::SummaryResponse, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let result: Result<_, crate::compaction::CompactionError> = async {
                cancellation
                    .check()
                    .map_err(|_| crate::compaction::CompactionError::Cancelled)?;
                let session = self
                    .sessions
                    .get(self.model.canonical_model.provider.clone())
                    .await?;
                let mut stream = session.call(request.into())?;
                let result = async {
                    let mut summary = crate::compaction::SummaryText::default();
                    while let Some(event) = stream.next(&cancellation).await? {
                        summary.observe(&event)?;
                        if matches!(event.base_event(), sessions::Event::Retry { .. }) {
                            self.events.observe(&event, &cancellation).await?;
                        }
                    }
                    Ok(summary.finish())
                }
                .await;
                stream.close().await?;
                result
            }
            .await;
            result.map_err(Into::into)
        })
    }

    fn invoke<'a, 'c>(
        &'a self,
        request: ProviderRequest,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<ProviderResponse, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let selected = &self.model.canonical_model;
            let mut state = self.state.lock().await;
            let request: sessions::Request = request.into();
            let estimate = crate::compaction::CompactionPlanner::estimate_input_tokens(
                selected,
                &request.instructions,
                &request.tools,
                &request.messages,
            )
            .map_err(|error| CoreError::from(TurnError::Planning(error)))?;
            state.request_accounting = Some(RequestAccounting {
                selected: selected.clone(),
                estimated_input_tokens: estimate,
            });
            let session = self
                .sessions
                .get(selected.provider.clone())
                .await
                .map_err(|error| CoreError::from(TurnError::Provider(error)))?;
            let output = OutputBridge(self.output);
            let activity = ActivityBridge(self.activity);
            let effects = StreamEffects {
                history: self.history,
                publisher: self.publisher,
                event_id: self.event_id,
                output: &output,
                activity: &activity,
            };
            let event = match Self::stream(
                effects,
                &mut *state,
                session.as_ref(),
                request,
                &cancellation,
            )
            .await
            {
                Ok(event) => event,
                Err(
                    error @ ProviderCallError::Provider(
                        sessions::Error::Http(
                            parrot_provider::http_stream::HttpError::RequestTooLarge(_),
                        )
                        | sessions::Error::Socket(
                            parrot_provider::responses_websocket::WebSocketError::RequestTooLarge {
                                ..
                            },
                        ),
                    ),
                ) => {
                    let mut error = CoreError::from(TurnError::ProviderCall(error));
                    error.kind = ErrorKind::RequestTooLarge;
                    return Err(error);
                }
                Err(error) => return Err(CoreError::from(TurnError::ProviderCall(error))),
            };
            match event.base_event() {
                sessions::Event::Completed(completed) => Ok(completed.clone().into()),
                _ => unreachable!("provider call returns completion"),
            }
        })
    }
}

impl RuntimeProvider<'_> {
    /// The caller owns session begin/close. This method owns and closes exactly
    /// the returned call stream, also after cancellation or a commit failure.
    /// As with other owned runtime operations, the caller must await completion
    /// rather than drop the future to cancel it.
    async fn stream(
        effects: StreamEffects<'_>,
        usage: &mut dyn ProviderUsage,
        session: &dyn Session,
        request: Request,
        cancellation: &dyn Cancellation,
    ) -> Result<sessions::Event, ProviderCallError> {
        let mut stream = session
            .call(request.clone())
            .map_err(ProviderCallError::Provider)?;
        let mut progress = StreamProgress::default();
        effects.activity.begin_provider_request();
        let mut result = Self::stream_events(
            &effects,
            usage,
            stream.as_mut(),
            &request,
            &mut progress,
            cancellation,
        )
        .await;
        if result.is_err() {
            // Already streamed text survives cancellation, including the held
            // suffix. Publication is deliberately uncancelled on this path.
            if let Err(error) =
                Self::commit(&effects, progress.fragments.end_stream(), &NeverCancelled).await
            {
                result = Err(error);
            }
        }
        // Never short-circuit cleanup: a stream close failure must not leave
        // activity marked running or omit the final output newline.
        let close = stream.close().await.map_err(ProviderCallError::Provider);
        let idle = Self::phase(
            &effects,
            protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
            progress.attempt,
        )
        .await;
        effects.activity.finish_provider_request();
        let newline = if progress.streamed_text {
            effects
                .output
                .append("\n")
                .await
                .map_err(ProviderCallError::Output)
        } else {
            Ok(())
        };
        close?;
        idle?;
        newline?;
        result
    }

    async fn stream_events(
        effects: &StreamEffects<'_>,
        usage: &mut dyn ProviderUsage,
        stream: &mut dyn EventStream,
        request: &Request,
        progress: &mut StreamProgress,
        cancellation: &dyn Cancellation,
    ) -> Result<sessions::Event, ProviderCallError> {
        let mut completed = sessions::Event::Completed(Completion::default());
        let mut awaiting_first_data = false;
        while let Some(event) =
            Self::next(effects, stream, &mut progress.fragments, cancellation).await?
        {
            let base = event.base_event();
            if !matches!(
                base,
                sessions::Event::TextDelta(_) | sessions::Event::ReasoningDelta { .. }
            ) {
                Self::commit(effects, progress.fragments.end_stream(), cancellation).await?;
            }
            if matches!(
                base,
                sessions::Event::HttpRequestStarted | sessions::Event::HttpResponseHeadersReceived
            ) {
                let phase = if matches!(base, sessions::Event::HttpRequestStarted) {
                    progress.attempt = progress.attempt.wrapping_add(1);
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING
                } else {
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED
                };
                awaiting_first_data = matches!(base, sessions::Event::HttpResponseHeadersReceived);
                Self::phase(effects, phase, progress.attempt).await?;
                continue;
            }
            if matches!(base, sessions::Event::Retry { .. }) {
                awaiting_first_data = false;
                Self::phase(
                    effects,
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                    progress.attempt,
                )
                .await?;
                if progress.streamed_text {
                    effects
                        .output
                        .append("\n[retry]\n")
                        .await
                        .map_err(ProviderCallError::Output)?;
                    progress.streamed_text = false;
                }
            }
            let first_data = match base {
                sessions::Event::EncryptedReasoningReceived => true,
                sessions::Event::TextDelta(_) | sessions::Event::ReasoningDelta { .. } => {
                    !event.text().is_empty()
                }
                sessions::Event::ToolCallDelta(call) => {
                    !call.id.is_empty() || !call.name.is_empty() || !event.text().is_empty()
                }
                _ => false,
            };
            if awaiting_first_data && first_data {
                awaiting_first_data = false;
                Self::phase(
                    effects,
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                    progress.attempt,
                )
                .await?;
            }
            if let sessions::Event::Completed(value) = base {
                usage
                    .observe_completion(request, value)
                    .await
                    .map_err(ProviderCallError::Storage)?;
                let mut published = Self::event(effects);
                published.set_provider_call_usage(protocol::ProviderCallUsage {
                    input_tokens: i64::from(value.input_tokens.max(0)),
                    output_tokens: i64::from(value.output_tokens.max(0)),
                    ..Default::default()
                });
                effects
                    .publisher
                    .publish_transient(published, &NeverCancelled)
                    .await
                    .map_err(ProviderCallError::Publication)?;
                completed = event;
                continue;
            }
            progress.streamed_text |=
                matches!(base, sessions::Event::TextDelta(_)) && !event.text().is_empty();
            effects.activity.observe_provider_event(&event);
            for ready in progress
                .fragments
                .add(translate_provider_event(Self::event(effects), &event))
            {
                Self::commit(effects, Some(ready), cancellation).await?;
            }
        }
        Self::commit(effects, progress.fragments.end_stream(), cancellation).await?;
        Ok(completed)
    }

    async fn next(
        effects: &StreamEffects<'_>,
        stream: &mut dyn EventStream,
        fragments: &mut StreamedFragmentBuffer,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<sessions::Event>, ProviderCallError> {
        let next = stream.next(cancellation);
        tokio::pin!(next);
        if let Some(deadline) = fragments.deadline() {
            tokio::select! {
                biased;
                result = &mut next => return result.map_err(ProviderCallError::Provider),
                _ = cancellation.cancelled() => return Err(ProviderCallError::Provider(sessions::Error::Cancelled)),
                _ = tokio::time::sleep_until(deadline) => {
                    Self::commit(effects, fragments.take(), cancellation).await?;
                }
            }
        }
        tokio::select! {
            biased;
            result = next => result.map_err(ProviderCallError::Provider),
            _ = cancellation.cancelled() => Err(ProviderCallError::Provider(sessions::Error::Cancelled)),
        }
    }

    async fn commit(
        effects: &StreamEffects<'_>,
        published: Option<Event>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), ProviderCallError> {
        let Some(published) = published else {
            return Ok(());
        };
        if published.has_text_chunk() && !published.text_chunk().fragment.is_empty() {
            effects
                .output
                .append(&published.text_chunk().fragment)
                .await
                .map_err(ProviderCallError::Output)?;
        }
        effects
            .history
            .append_message(published.clone(), None, ConversationOrigin::Model)
            .await
            .map_err(ProviderCallError::Storage)?;
        effects
            .publisher
            .publish_committed(&Committed::after_commit(published), cancellation)
            .await
            .map_err(ProviderCallError::Publication)
    }

    fn event(effects: &StreamEffects<'_>) -> Event {
        Event {
            id: (effects.event_id)(),
            agent_session_id: effects.history.agent_session_id().into(),
            ..Default::default()
        }
    }

    async fn phase(
        effects: &StreamEffects<'_>,
        phase: protocol::ProviderRequestPhase,
        attempt: u32,
    ) -> Result<(), ProviderCallError> {
        let mut event = Self::event(effects);
        event.set_provider_request_phase_changed(protocol::ProviderRequestPhaseChangedEvent {
            phase: phase.into(),
            attempt,
            ..Default::default()
        });
        effects
            .publisher
            .publish_transient(event, &NeverCancelled)
            .await
            .map_err(ProviderCallError::Publication)
    }
}

#[cfg(test)]
mod tests;

struct OutputBridge<'a>(&'a dyn SessionOutput);
impl OutputWriter for OutputBridge<'_> {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        self.0.append(text)
    }
}
struct ActivityBridge<'a>(&'a dyn SessionActivity);
impl ProviderActivity for ActivityBridge<'_> {
    fn begin_provider_request(&self) {
        self.0.begin_provider_request();
    }
    fn observe_provider_event(&self, event: &sessions::Event) {
        self.0.observe_provider_event(event);
    }
    fn finish_provider_request(&self) {
        self.0.finish_provider_request();
    }
}
