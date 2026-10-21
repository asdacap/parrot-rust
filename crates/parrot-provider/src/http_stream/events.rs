use super::{
    BoundedStream, ByteStream, Endpoint, HttpError, HttpTransport, IdleTimeoutStream,
    MAX_EVENT_BYTES, ResponseByteCounter, SseEvent, SseStream, StreamOptions, open_stream,
};
use parrot_domain::contracts::async_ops::Cancellation;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpStreamEvent {
    RequestStarted,
    ResponseHeadersReceived,
    Event(SseEvent),
}

type BodyEvents = SseStream<BoundedStream<IdleTimeoutStream<Box<dyn ByteStream>>>>;
type PendingRequest = (Endpoint, Vec<u8>, Vec<(String, String)>);

/// The HTTP lifecycle prefix and owning SSE stream used by wire adapters.
/// No retry or WebSocket fallback is performed by this layer.
pub struct HttpEventStream {
    transport: Arc<dyn HttpTransport>,
    request: Option<PendingRequest>,
    options: StreamOptions,
    counter: ResponseByteCounter,
    body: Option<BodyEvents>,
    stage: u8,
    turn_state: Option<String>,
}
impl HttpEventStream {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        endpoint: impl Into<Endpoint>,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
        options: StreamOptions,
        counter: ResponseByteCounter,
    ) -> Result<Self, HttpError> {
        if body.len() > options.maximum_request_bytes {
            return Err(HttpError::RequestTooLarge(options.maximum_request_bytes));
        }
        Ok(Self {
            transport,
            request: Some((endpoint.into(), body, headers)),
            options,
            counter,
            body: None,
            stage: 0,
            turn_state: None,
        })
    }
    pub fn turn_state(&self) -> Option<&str> {
        self.turn_state.as_deref()
    }
    pub fn response_bytes(&self) -> u64 {
        self.counter.get()
    }
    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<HttpStreamEvent>, HttpError> {
        if self.stage == 0 {
            self.stage = 1;
            return Ok(Some(HttpStreamEvent::RequestStarted));
        }
        if self.stage == 1 {
            self.stage = 3;
            let (endpoint, body, headers) =
                self.request.take().expect("HTTP request consumed once");
            match open_stream(
                &*self.transport,
                endpoint,
                body,
                headers,
                self.options,
                self.counter.clone(),
                cancellation,
            )
            .await
            {
                Ok(response) => {
                    self.stage = 2;
                    self.turn_state = response.turn_state().map(str::to_owned);
                    self.body = Some(SseStream::new(response.content, MAX_EVENT_BYTES as i32));
                    return Ok(Some(HttpStreamEvent::ResponseHeadersReceived));
                }
                Err(error) => {
                    self.stage = 3;
                    return Err(error);
                }
            }
        }
        if self.stage == 3 {
            return Ok(None);
        }
        let result = self
            .body
            .as_mut()
            .expect("body present after headers")
            .next(cancellation)
            .await;
        match result {
            Ok(Some(event)) => Ok(Some(HttpStreamEvent::Event(event))),
            Ok(None) => {
                self.body = None;
                self.stage = 3;
                Ok(None)
            }
            Err(error) => {
                self.body = None;
                self.stage = 3;
                Err(error)
            }
        }
    }
}
