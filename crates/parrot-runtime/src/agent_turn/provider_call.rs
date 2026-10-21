//! One provider stream, with durable fragment publication and awaited cleanup.

use std::{fmt, io, time::Duration};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
};
use parrot_provider::sessions::{self, Completion, EventStream, Request, Session};
use parrot_storage::{
    conversation::AgentHistoryView,
    events::{EventError, NeverCancelled},
};
use tokio::time::Instant;

use crate::event_broker::BrokerError;

pub trait OutputWriter: Send + Sync {
    fn append<'a>(&'a self, text: &'a str) -> BoxFuture<'a, io::Result<()>>;
}

/// The session's activity tracker; HTTP phase and completion events are not
/// observed, matching the provider request activity boundary in the source.
pub trait ProviderActivity: Send + Sync {
    fn begin_provider_request(&self);
    fn observe_provider_event(&self, event: &sessions::Event);
    fn finish_provider_request(&self);
}

/// The caller supplies actual token-budget calibration and durable request
/// usage recording. Request construction and budget calculation remain outside
/// this component; each completion, not just the last, is observed here.
pub trait ProviderUsage: Send + Sync {
    fn observe_completion<'a>(
        &'a mut self,
        request: &'a Request,
        completed: &'a Completion,
    ) -> BoxFuture<'a, Result<(), EventError>>;
}

#[derive(Debug)]
pub enum ProviderCallError {
    Provider(sessions::Error),
    Storage(EventError),
    Publication(OperationError<BrokerError>),
    Output(io::Error),
}
impl fmt::Display for ProviderCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
            Self::Publication(error) => error.fmt(formatter),
            Self::Output(error) => error.fmt(formatter),
        }
    }
}
impl std::error::Error for ProviderCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Provider(error) => error,
            Self::Storage(error) => error,
            Self::Publication(error) => error,
            Self::Output(error) => error,
        })
    }
}

pub struct ProviderCall<'a> {
    pub history: &'a AgentHistoryView,
    pub publisher: &'a dyn EventPublisher<Event = Event, Error = BrokerError>,
    pub event_id: &'a (dyn Fn() -> String + Send + Sync),
    pub output: &'a dyn OutputWriter,
    pub activity: &'a dyn ProviderActivity,
    pub usage: &'a mut dyn ProviderUsage,
}

impl ProviderCall<'_> {
    /// The caller owns session begin/close. This method owns and closes exactly
    /// the returned call stream, also after cancellation or a commit failure.
    /// As with other owned runtime operations, the caller must await completion
    /// rather than drop the future to cancel it.
    pub async fn stream(
        &mut self,
        session: &dyn Session,
        request: Request,
        cancellation: &dyn Cancellation,
    ) -> Result<sessions::Event, ProviderCallError> {
        let mut stream = session
            .call(request.clone())
            .map_err(ProviderCallError::Provider)?;
        let mut fragments = StreamedFragmentBuffer::default();
        let mut attempt = 0;
        let mut streamed_text = false;
        self.activity.begin_provider_request();
        let mut result = self
            .stream_events(
                stream.as_mut(),
                &request,
                &mut fragments,
                &mut attempt,
                &mut streamed_text,
                cancellation,
            )
            .await;
        if result.is_err() {
            // Already streamed text survives cancellation, including the held
            // suffix. Publication is deliberately uncancelled on this path.
            if let Err(error) = self.commit(fragments.end_stream(), &NeverCancelled).await {
                result = Err(error);
            }
        }
        // Never short-circuit cleanup: a stream close failure must not leave
        // activity marked running or omit the final output newline.
        let close = stream.close().await.map_err(ProviderCallError::Provider);
        let idle = self
            .phase(
                protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                attempt,
            )
            .await;
        self.activity.finish_provider_request();
        let newline = if streamed_text {
            self.output
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
        &mut self,
        stream: &mut dyn EventStream,
        request: &Request,
        fragments: &mut StreamedFragmentBuffer,
        attempt: &mut u32,
        streamed_text: &mut bool,
        cancellation: &dyn Cancellation,
    ) -> Result<sessions::Event, ProviderCallError> {
        let mut completed = sessions::Event::Completed(Completion::default());
        let mut awaiting_first_data = false;
        while let Some(event) = self.next(stream, fragments, cancellation).await? {
            let base = event.base_event();
            if !matches!(
                base,
                sessions::Event::TextDelta(_) | sessions::Event::ReasoningDelta { .. }
            ) {
                self.commit(fragments.end_stream(), cancellation).await?;
            }
            if matches!(
                base,
                sessions::Event::HttpRequestStarted | sessions::Event::HttpResponseHeadersReceived
            ) {
                let phase = if matches!(base, sessions::Event::HttpRequestStarted) {
                    *attempt = attempt.wrapping_add(1);
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING
                } else {
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED
                };
                awaiting_first_data = matches!(base, sessions::Event::HttpResponseHeadersReceived);
                self.phase(phase, *attempt).await?;
                continue;
            }
            if matches!(base, sessions::Event::Retry { .. }) {
                awaiting_first_data = false;
                self.phase(
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                    *attempt,
                )
                .await?;
                if *streamed_text {
                    self.output
                        .append("\n[retry]\n")
                        .await
                        .map_err(ProviderCallError::Output)?;
                    *streamed_text = false;
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
                self.phase(
                    protocol::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_IDLE,
                    *attempt,
                )
                .await?;
            }
            if let sessions::Event::Completed(value) = base {
                self.usage
                    .observe_completion(request, value)
                    .await
                    .map_err(ProviderCallError::Storage)?;
                let mut published = self.event();
                published.set_provider_call_usage(protocol::ProviderCallUsage {
                    input_tokens: i64::from(value.input_tokens.max(0)),
                    output_tokens: i64::from(value.output_tokens.max(0)),
                    ..Default::default()
                });
                self.publisher
                    .publish_transient(published, &NeverCancelled)
                    .await
                    .map_err(ProviderCallError::Publication)?;
                completed = event;
                continue;
            }
            *streamed_text |=
                matches!(base, sessions::Event::TextDelta(_)) && !event.text().is_empty();
            self.activity.observe_provider_event(&event);
            for ready in fragments.add(translate_provider_event(self.event(), &event)) {
                self.commit(Some(ready), cancellation).await?;
            }
        }
        self.commit(fragments.end_stream(), cancellation).await?;
        Ok(completed)
    }

    async fn next(
        &self,
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
                    self.commit(fragments.take(), cancellation).await?;
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
        &self,
        published: Option<Event>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), ProviderCallError> {
        let Some(published) = published else {
            return Ok(());
        };
        if published.has_text_chunk() && !published.text_chunk().fragment.is_empty() {
            self.output
                .append(&published.text_chunk().fragment)
                .await
                .map_err(ProviderCallError::Output)?;
        }
        persist_and_publish(self.history, self.publisher, published, cancellation).await
    }

    fn event(&self) -> Event {
        Event {
            id: (self.event_id)(),
            agent_session_id: self.history.agent_session_id().into(),
            ..Default::default()
        }
    }

    async fn phase(
        &self,
        phase: protocol::ProviderRequestPhase,
        attempt: u32,
    ) -> Result<(), ProviderCallError> {
        let mut event = self.event();
        event.set_provider_request_phase_changed(protocol::ProviderRequestPhaseChangedEvent {
            phase: phase.into(),
            attempt,
            ..Default::default()
        });
        self.publisher
            .publish_transient(event, &NeverCancelled)
            .await
            .map_err(ProviderCallError::Publication)
    }
}

/// Reports a retry from a provider call made outside a turn, such as compaction.
/// No stream, output writer, activity tracker or usage recorder is required.
pub async fn emit_retry(
    history: &AgentHistoryView,
    publisher: &dyn EventPublisher<Event = Event, Error = BrokerError>,
    event_id: &(dyn Fn() -> String + Send + Sync),
    event: &sessions::Event,
    cancellation: &dyn Cancellation,
) -> Result<(), ProviderCallError> {
    let published = translate_provider_event(
        Event {
            id: event_id(),
            agent_session_id: history.agent_session_id().into(),
            ..Default::default()
        },
        event,
    );
    persist_and_publish(history, publisher, published, cancellation).await
}

async fn persist_and_publish(
    history: &AgentHistoryView,
    publisher: &dyn EventPublisher<Event = Event, Error = BrokerError>,
    published: Event,
    cancellation: &dyn Cancellation,
) -> Result<(), ProviderCallError> {
    history
        .append(published.clone(), None, None)
        .await
        .map_err(ProviderCallError::Storage)?;
    publisher
        .publish_committed(&Committed::after_commit(published), cancellation)
        .await
        .map_err(ProviderCallError::Publication)
}

fn translate_provider_event(mut published: Event, event: &sessions::Event) -> Event {
    match event.base_event() {
        sessions::Event::TextDelta(_) => published.set_text_chunk(protocol::TextChunk {
            fragment: event.text().into(),
            ..Default::default()
        }),
        sessions::Event::ReasoningDelta {
            kind,
            part_id,
            completed,
            ..
        } => published.set_reasoning_chunk(protocol::ReasoningChunk {
            fragment: event.text().into(),
            kind: match kind {
                sessions::ReasoningKind::Raw => protocol::ReasoningKind::REASONING_KIND_RAW,
                sessions::ReasoningKind::Summary => protocol::ReasoningKind::REASONING_KIND_SUMMARY,
            }
            .into(),
            part_id: part_id.clone(),
            completed: *completed,
            ..Default::default()
        }),
        sessions::Event::EncryptedReasoningReceived => published
            .set_encrypted_reasoning_received(protocol::EncryptedReasoningReceivedEvent::default()),
        sessions::Event::ToolCallDelta(call) => {
            published.set_tool_call_chunk(protocol::ToolCallChunk {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments_fragment: event.text().into(),
                ..Default::default()
            })
        }
        sessions::Event::Retry {
            attempt,
            retry_after,
            ..
        } => published.set_retry_notice(protocol::RetryNotice {
            attempt: *attempt,
            retry_after_ms: retry_after.as_millis() as i32,
            reason: event.text().into(),
            ..Default::default()
        }),
        _ => unreachable!("phase and completion events are handled before translation"),
    }
    published
}

#[derive(Clone, Eq, PartialEq)]
enum FragmentStream {
    Text,
    Reasoning(protobuf::EnumOrUnknown<protocol::ReasoningKind>, String),
}
impl FragmentStream {
    fn of(event: &Event) -> Option<Self> {
        if event.has_text_chunk() {
            Some(Self::Text)
        } else if event.has_reasoning_chunk() {
            Some(Self::Reasoning(
                event.reasoning_chunk().kind,
                event.reasoning_chunk().part_id.clone(),
            ))
        } else {
            None
        }
    }
    fn close(self, event: &Event) -> Option<Self> {
        if event.has_reasoning_chunk() && event.reasoning_chunk().completed {
            None
        } else {
            Some(self)
        }
    }
}

#[derive(Default)]
struct StreamedFragmentBuffer {
    stream: Option<FragmentStream>,
    held: Option<Event>,
    fragment: String,
    held_since: Option<Instant>,
}
impl StreamedFragmentBuffer {
    fn deadline(&self) -> Option<Instant> {
        self.held_since
            .map(|since| since + Duration::from_millis(500))
    }
    fn add(&mut self, published: Event) -> Vec<Event> {
        let stream = FragmentStream::of(&published);
        if stream.is_none() || stream != self.stream {
            let mut ready = self.end_stream().into_iter().collect::<Vec<_>>();
            self.stream = stream.and_then(|stream| stream.close(&published));
            ready.push(published);
            return ready;
        }
        if self.held.is_none() {
            self.held = Some(published.clone());
            self.held_since = Some(Instant::now());
        } else if published.has_reasoning_chunk() {
            self.held
                .as_mut()
                .expect("held fragment")
                .mut_reasoning_chunk()
                .completed = published.reasoning_chunk().completed;
        }
        self.fragment.push_str(if published.has_text_chunk() {
            &published.text_chunk().fragment
        } else {
            &published.reasoning_chunk().fragment
        });
        self.stream = stream.and_then(|stream| stream.close(&published));
        if self.stream.is_none()
            || self
                .deadline()
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.take().into_iter().collect()
        } else {
            Vec::new()
        }
    }
    fn take(&mut self) -> Option<Event> {
        let mut held = self.held.take()?;
        if held.has_text_chunk() {
            held.mut_text_chunk().fragment = std::mem::take(&mut self.fragment);
        } else {
            held.mut_reasoning_chunk().fragment = std::mem::take(&mut self.fragment);
        }
        self.held_since = None;
        Some(held)
    }
    fn end_stream(&mut self) -> Option<Event> {
        self.stream = None;
        self.take()
    }
}
