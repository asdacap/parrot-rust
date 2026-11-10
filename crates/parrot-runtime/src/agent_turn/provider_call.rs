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
use parrot_provider::sessions::{self, Completion, Request};
use parrot_storage::{conversation::AgentHistoryView, events::EventError};
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

/// Reports every actual completion to the caller's accounting owner before
/// publishing usage, including intermediate completions before a later failure.
/// Request construction and token-budget calibration remain outside this component.
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

pub(crate) fn translate_provider_event(mut published: Event, event: &sessions::Event) -> Event {
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
pub(crate) struct StreamedFragmentBuffer {
    stream: Option<FragmentStream>,
    held: Option<Event>,
    fragment: String,
    held_since: Option<Instant>,
}
impl StreamedFragmentBuffer {
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.held_since
            .map(|since| since + Duration::from_millis(500))
    }
    pub(crate) fn add(&mut self, published: Event) -> Vec<Event> {
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
    pub(crate) fn take(&mut self) -> Option<Event> {
        let mut held = self.held.take()?;
        if held.has_text_chunk() {
            held.mut_text_chunk().fragment = std::mem::take(&mut self.fragment);
        } else {
            held.mut_reasoning_chunk().fragment = std::mem::take(&mut self.fragment);
        }
        self.held_since = None;
        Some(held)
    }
    pub(crate) fn end_stream(&mut self) -> Option<Event> {
        self.stream = None;
        self.take()
    }
}
