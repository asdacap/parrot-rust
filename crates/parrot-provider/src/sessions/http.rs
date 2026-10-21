//! Owning HTTP/SSE composition over the protocol adapters and T25 transport.
use super::diagnostics::{CallTrace, FailureKind, RequestTrace, TraceOutcome};
use super::{Error, Event, EventStream};
use crate::{
    chat_completions::{ChatStream, ChatStreamError},
    http_stream::{
        Endpoint, HttpError, HttpEventStream, HttpRequest, HttpResponse, HttpStreamEvent,
        HttpTransport, ResponseByteCounter, StreamOptions,
    },
    responses::{ResponsesEvent, ResponsesParser},
    responses_websocket::{HttpEvents, SessionEvent, WebSocketError},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    ChatCompletions,
    Responses,
}

/// Raw Responses events retain the adapter's completion and native replay data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpResponseEvent {
    RequestStarted,
    ResponseHeadersReceived,
    Event(ResponsesEvent),
}
impl From<HttpResponseEvent> for Event {
    fn from(event: HttpResponseEvent) -> Self {
        match event {
            HttpResponseEvent::RequestStarted => Self::HttpRequestStarted,
            HttpResponseEvent::ResponseHeadersReceived => Self::HttpResponseHeadersReceived,
            HttpResponseEvent::Event(event) => event.into(),
        }
    }
}

struct ObservedTransport {
    inner: Arc<dyn HttpTransport>,
    obtained: Arc<AtomicBool>,
}
impl HttpTransport for ObservedTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let response = self.inner.send(request, cancellation).await?;
            self.obtained.store(true, Ordering::Release);
            Ok(response)
        })
    }
}
struct InitialRequest {
    transport: Arc<dyn HttpTransport>,
    endpoint: Endpoint,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    options: StreamOptions,
    provider_id: String,
}
struct ResponseReader {
    http: HttpEventStream,
    parser: ResponsesParser,
    pending: VecDeque<ResponsesEvent>,
    terminal: bool,
}
enum Reader {
    Chat(Box<ChatStream>),
    Responses(Box<ResponseReader>),
}

pub struct HttpDriver {
    protocol: Protocol,
    initial: Option<InitialRequest>,
    reader: Option<Reader>,
    trace: Option<Arc<CallTrace>>,
    attempt: Option<RequestTrace>,
    counter: ResponseByteCounter,
    obtained: Arc<AtomicBool>,
    accounted_bytes: u64,
    turn_state: Option<String>,
    turn_state_captured: bool,
    done: bool,
}
impl HttpDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        endpoint: Endpoint,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        options: StreamOptions,
        protocol: Protocol,
        provider_id: String,
        _include_router_metadata: bool,
        trace: Option<Arc<CallTrace>>,
    ) -> Self {
        Self {
            protocol,
            initial: Some(InitialRequest {
                transport,
                endpoint,
                headers,
                body,
                options,
                provider_id,
            }),
            reader: None,
            trace,
            attempt: None,
            counter: ResponseByteCounter::default(),
            obtained: Arc::new(AtomicBool::new(false)),
            accounted_bytes: 0,
            turn_state: None,
            turn_state_captured: false,
            done: false,
        }
    }
    pub fn take_turn_state(&mut self) -> Option<String> {
        self.turn_state.take()
    }
    pub fn response_bytes(&self) -> u64 {
        self.counter.get()
    }
    /// The scope owner can cancel this trace even while no read is outstanding.
    pub fn request_trace(&self) -> Option<RequestTrace> {
        self.attempt.clone()
    }

    fn initialize(&mut self) -> Result<(), Error> {
        let Some(initial) = self.initial.take() else {
            return Ok(());
        };
        // Source validates the actual encoded size before tracing or dumping.
        if initial.body.len() > initial.options.maximum_request_bytes {
            return Err(HttpError::RequestTooLarge(initial.options.maximum_request_bytes).into());
        }
        if let Some(trace) = &self.trace {
            static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
            let attempt = trace.begin_request(
                "http_sse",
                &format!("{:032x}", NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)),
            );
            let counter = self.counter.clone();
            let obtained = self.obtained.clone();
            attempt.track_response_bytes(Arc::new(move || {
                obtained.load(Ordering::Acquire).then(|| counter.get())
            }));
            attempt.record_request_bytes(initial.body.len() as u64);
            trace.dump_prepared_body(&initial.body);
            self.attempt = Some(attempt);
        }
        let transport = Arc::new(ObservedTransport {
            inner: initial.transport,
            obtained: self.obtained.clone(),
        });
        let http = HttpEventStream::new(
            transport,
            initial.endpoint,
            initial.body,
            initial.headers,
            initial.options,
            self.counter.clone(),
        )?;
        self.reader = Some(match self.protocol {
            Protocol::ChatCompletions => {
                Reader::Chat(Box::new(ChatStream::new(http, initial.provider_id)))
            }
            Protocol::Responses => Reader::Responses(Box::new(ResponseReader {
                http,
                parser: ResponsesParser::new(initial.provider_id),
                pending: VecDeque::new(),
                terminal: false,
            })),
        });
        Ok(())
    }
    fn account(&mut self) {
        let current = self.counter.get();
        if let Some(attempt) = &self.attempt {
            if self.obtained.load(Ordering::Acquire) {
                attempt.mark_response_obtained();
            }
            if current > self.accounted_bytes {
                attempt.record_response_bytes(current - self.accounted_bytes);
            }
        }
        self.accounted_bytes = current;
        let state = match &self.reader {
            Some(Reader::Chat(reader)) => reader.turn_state(),
            Some(Reader::Responses(reader)) => reader.http.turn_state(),
            None => None,
        };
        if !self.turn_state_captured {
            if let Some(state) = state {
                self.turn_state = Some(state.to_owned());
                self.turn_state_captured = true;
            }
        }
    }
    fn finish(&mut self, outcome: TraceOutcome) {
        self.account();
        if let Some(attempt) = &self.attempt {
            attempt.finish(outcome);
        }
        self.reader = None;
        self.initial = None;
        self.done = true;
    }
    fn settle<T>(&mut self, result: Result<Option<T>, Error>) -> Result<Option<T>, Error> {
        self.account();
        match &result {
            Ok(None) => self.finish(TraceOutcome::Completed),
            Err(error) => self.finish(TraceOutcome::Failed(failure_kind(error))),
            Ok(Some(_)) => {}
        }
        result
    }
    pub async fn next_response(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<HttpResponseEvent>, Error> {
        if self.protocol != Protocol::Responses {
            return Err(Error::Provider(
                "Responses events require the Responses protocol".into(),
            ));
        }
        if self.done {
            return Ok(None);
        }
        let result = async {
            self.initialize()?;
            if let Some(attempt) = &self.attempt {
                attempt.observe_cancellation(cancellation.is_cancelled());
            }
            let Some(Reader::Responses(reader)) = self.reader.as_mut() else {
                unreachable!("Responses reader initialized")
            };
            let ResponseReader {
                http,
                parser,
                pending,
                terminal,
            } = reader.as_mut();
            loop {
                if let Some(event) = pending.pop_front() {
                    return Ok(Some(HttpResponseEvent::Event(event)));
                }
                if *terminal {
                    return Ok(None);
                }
                match http.next(cancellation).await? {
                    Some(HttpStreamEvent::RequestStarted) => {
                        return Ok(Some(HttpResponseEvent::RequestStarted));
                    }
                    Some(HttpStreamEvent::ResponseHeadersReceived) => {
                        return Ok(Some(HttpResponseEvent::ResponseHeadersReceived));
                    }
                    Some(HttpStreamEvent::Event(record)) => {
                        let events = parser.push_data(&record.data).map_err(Error::Response)?;
                        *terminal = events
                            .iter()
                            .any(|event| matches!(event, ResponsesEvent::Completed(_)));
                        pending.extend(events);
                    }
                    None => {
                        pending.extend(parser.finish().map_err(Error::Response)?);
                        *terminal = true;
                    }
                }
            }
        }
        .await;
        self.settle(result)
    }
    async fn next_common(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<Event>, Error> {
        if self.protocol == Protocol::Responses {
            return self
                .next_response(cancellation)
                .await
                .map(|event| event.map(Into::into));
        }
        if self.done {
            return Ok(None);
        }
        let result = async {
            self.initialize()?;
            if let Some(attempt) = &self.attempt {
                attempt.observe_cancellation(cancellation.is_cancelled());
            }
            let Some(Reader::Chat(reader)) = self.reader.as_mut() else {
                unreachable!("Chat reader initialized")
            };
            reader
                .next(cancellation)
                .await
                .map(|event| event.map(Into::into))
                .map_err(chat_error)
        }
        .await;
        self.settle(result)
    }
}
impl EventStream for HttpDriver {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(self.next_common(cancellation))
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if !self.done {
                self.finish(TraceOutcome::Disposed);
            }
            Ok(())
        })
    }
}
impl Drop for HttpDriver {
    fn drop(&mut self) {
        if !self.done {
            self.finish(TraceOutcome::Disposed);
        }
    }
}
fn chat_error(error: ChatStreamError) -> Error {
    match error {
        ChatStreamError::Http(error) => Error::from(error),
        ChatStreamError::Provider(error) => Error::ChatProvider(error),
        ChatStreamError::UnexpectedEof => Error::Wire(ChatStreamError::UnexpectedEof.to_string()),
        other => Error::Encode(other.to_string()),
    }
}
fn failure_kind(error: &Error) -> FailureKind {
    match error {
        Error::Cancelled
        | Error::Cancellation(_)
        | Error::Http(HttpError::Cancelled | HttpError::RequestCancelled(_))
        | Error::Socket(WebSocketError::Cancelled) => FailureKind::Cancelled,
        Error::Http(HttpError::RequestTooLarge(_)) => FailureKind::RequestTooLarge,
        Error::Http(HttpError::Wire(_))
        | Error::Wire(_)
        | Error::Response(crate::responses::ResponsesError::WireProtocol(_)) => {
            FailureKind::WireProtocol
        }
        Error::Http(HttpError::Transport(_)) => FailureKind::Http,
        Error::Disposed | Error::InvalidOperation(_) | Error::Http(HttpError::Disposed) => {
            FailureKind::InvalidOperation
        }
        Error::Io(_) => FailureKind::Io,
        _ => FailureKind::Unexpected,
    }
}

/// T29's error type does not carry HTTP status/body details. The owning session
/// checks this shared slot when its call fails, retaining the actual error.
pub type HttpErrorSlot = Arc<Mutex<Option<Error>>>;
pub struct HttpEventsBridge {
    driver: HttpDriver,
    errors: HttpErrorSlot,
}
impl HttpEventsBridge {
    pub fn new(driver: HttpDriver, errors: HttpErrorSlot) -> Self {
        Self { driver, errors }
    }
    fn capture(&self, error: Error) -> WebSocketError {
        let mapped = match &error {
            Error::Response(error) => WebSocketError::Response(error.clone()),
            Error::Http(HttpError::Cancelled) | Error::Cancelled => WebSocketError::Cancelled,
            Error::Http(HttpError::RequestTooLarge(maximum)) => {
                WebSocketError::RequestTooLarge { maximum: *maximum }
            }
            _ => WebSocketError::Wire(error.to_string()),
        };
        *self
            .errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
        mapped
    }
}
impl HttpEvents for HttpEventsBridge {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        Box::pin(async move {
            loop {
                match self
                    .driver
                    .next_response(cancellation)
                    .await
                    .map_err(|error| self.capture(error))?
                {
                    Some(HttpResponseEvent::Event(event)) => return Ok(Some(event)),
                    Some(_) => {}
                    None => return Ok(None),
                }
            }
        })
    }
    fn next_event<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<SessionEvent>, WebSocketError>> {
        Box::pin(async move {
            self.driver
                .next_response(cancellation)
                .await
                .map(|event| {
                    event.map(|event| match event {
                        HttpResponseEvent::RequestStarted => SessionEvent::HttpRequestStarted,
                        HttpResponseEvent::ResponseHeadersReceived => {
                            SessionEvent::HttpResponseHeadersReceived
                        }
                        HttpResponseEvent::Event(event) => SessionEvent::Response(event),
                    })
                })
                .map_err(|error| self.capture(error))
        })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        self.driver.take_turn_state()
    }
}
