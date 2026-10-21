use super::observer::{WebSocketAttemptObserver, WebSocketAttemptOutcome, WebSocketObserver};
use super::transport::ExchangeState;
use super::{
    Headers, ResponsesWebSocket, SocketState, WebSocketClient, WebSocketError, WebSocketRequest,
};
use crate::responses::{ResponsesCompletion, ResponsesError, ResponsesEvent};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

/// Authentication is obtained for each new connection, never retained by the session.
pub trait HeaderSource: Send + Sync {
    fn headers<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Headers, WebSocketError>>;
}

/// The provider's HTTP implementation remains responsible for HTTP parsing and policy.
pub trait HttpFallback: Send + Sync {
    fn open_unprepared<'a>(
        &'a self,
        _turn_state: String,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
        Box::pin(async {
            Err(WebSocketError::InvalidOperation(
                "HTTP fallback does not support unprepared requests.".into(),
            ))
        })
    }

    fn open<'a>(
        &'a self,
        body: Vec<u8>,
        turn_state: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>>;
}
pub trait HttpEvents: Send {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>>;
    /// Override to preserve HTTP lifecycle markers, advancing only one event per pull.
    fn next_event<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<SessionEvent>, WebSocketError>> {
        Box::pin(async move {
            self.next(cancellation)
                .await
                .map(|event| event.map(SessionEvent::Response))
        })
    }
    fn take_turn_state(&mut self) -> Option<String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    HttpRequestStarted,
    HttpResponseHeadersReceived,
    Response(ResponsesEvent),
    Retry { attempt: u32, reason: String },
}

struct CompletedResponse {
    settings: [u8; 32],
    response_id: String,
    items: Vec<[u8; 32]>,
}

/// Connection-local continuation state. A mutable call borrow serializes access.
pub struct Session {
    provider_id: String,
    headers: Arc<dyn HeaderSource>,
    client: WebSocketClient,
    connection: Option<ResponsesWebSocket>,
    completed: Option<CompletedResponse>,
    turn_state: Option<String>,
    http_only: bool,
    reset_connection: bool,
    disposed: bool,
}
impl Session {
    /// WebSockets require an explicit `disable_websocket = false` opt-in.
    pub fn new(
        provider_id: impl Into<String>,
        headers: Arc<dyn HeaderSource>,
        client: WebSocketClient,
        disable_websocket: bool,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            headers,
            client,
            connection: None,
            completed: None,
            turn_state: None,
            http_only: disable_websocket,
            reset_connection: false,
            disposed: false,
        }
    }
    pub fn http_default(
        provider_id: impl Into<String>,
        headers: Arc<dyn HeaderSource>,
        client: WebSocketClient,
    ) -> Self {
        Self::new(provider_id, headers, client, true)
    }
    pub fn begin_turn(&mut self) {
        self.turn_state = None;
        self.reset_connection = true;
    }
    pub fn try_fall_back_to_http(&mut self) -> Result<bool, WebSocketError> {
        self.check_disposed()?;
        self.select_http();
        Ok(true)
    }
    pub fn dispose(&mut self) {
        self.disposed = true;
        self.completed = None;
        self.poison();
    }
    pub fn call<'a>(
        &'a mut self,
        request: WebSocketRequest,
        http: &'a dyn HttpFallback,
    ) -> Result<SessionCall<'a>, WebSocketError> {
        self.check_disposed()?;
        self.reset_stale_connection();
        let mut call = SessionCall::new(self, http);
        call.prepare(request);
        Ok(call)
    }
    /// Prepares only on the first WebSocket pull; HTTP uses `open_unprepared` instead.
    pub fn call_lazy<'a>(
        &'a mut self,
        prepare: impl FnOnce() -> Result<WebSocketRequest, WebSocketError> + Send + 'a,
        http: &'a dyn HttpFallback,
    ) -> Result<SessionCall<'a>, WebSocketError> {
        self.check_disposed()?;
        let mut call = SessionCall::new(self, http);
        call.use_original_http = true;
        call.prepare_request = Some(Box::new(prepare));
        Ok(call)
    }
    fn reset_stale_connection(&mut self) {
        if !self.http_only
            && (self.reset_connection
                || self
                    .connection
                    .as_ref()
                    .is_some_and(ResponsesWebSocket::stale))
        {
            self.completed = None;
            self.poison();
            self.reset_connection = false;
        }
    }
    fn check_disposed(&self) -> Result<(), WebSocketError> {
        if self.disposed {
            Err(WebSocketError::Disposed)
        } else {
            Ok(())
        }
    }
    fn poison(&mut self) {
        self.connection = None;
    }
    fn select_http(&mut self) {
        self.http_only = true;
        self.completed = None;
        self.poison();
    }
    fn capture_turn_state(&mut self, state: Option<String>) {
        if self.turn_state.is_none() {
            self.turn_state = state.filter(|state| !state.is_empty());
        }
    }
}

/// One pull advances only as far as the next event. Visible output is never replayed.
pub struct SessionCall<'a> {
    session: &'a mut Session,
    http: &'a dyn HttpFallback,
    request: Option<WebSocketRequest>,
    use_original_http: bool,
    prepare_request:
        Option<Box<dyn FnOnce() -> Result<WebSocketRequest, WebSocketError> + Send + 'a>>,
    observer: Option<Arc<dyn WebSocketObserver>>,
    attempt_observer: Option<Box<dyn WebSocketAttemptObserver>>,
    attempt_active: bool,
    settings: [u8; 32],
    input_hashes: Vec<[u8; 32]>,
    previous_id: String,
    skip: usize,
    exchange: Option<ExchangeState>,
    http_events: Option<Box<dyn HttpEvents>>,
    visible: bool,
    recovered: bool,
    done: bool,
    started: bool,
}
impl<'a> SessionCall<'a> {
    fn new(session: &'a mut Session, http: &'a dyn HttpFallback) -> Self {
        Self {
            session,
            http,
            request: None,
            use_original_http: false,
            prepare_request: None,
            observer: None,
            attempt_observer: None,
            attempt_active: false,
            settings: [0; 32],
            input_hashes: Vec::new(),
            previous_id: String::new(),
            skip: 0,
            exchange: None,
            http_events: None,
            visible: false,
            recovered: false,
            done: false,
            started: false,
        }
    }
    pub fn with_websocket_observer(mut self, observer: Arc<dyn WebSocketObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
    fn prepare(&mut self, request: WebSocketRequest) {
        let settings = hash(&request.settings());
        let input_hashes: Vec<_> = request
            .input()
            .iter()
            .map(|item| hash(item.get().as_bytes()))
            .collect();
        let previous = self.session.completed.take();
        let (previous_id, skip) = match previous {
            Some(previous)
                if !self.session.http_only
                    && previous.settings == settings
                    && input_hashes.len() > previous.items.len()
                    && input_hashes.starts_with(&previous.items) =>
            {
                (previous.response_id, previous.items.len())
            }
            _ => (String::new(), 0),
        };
        self.settings = settings;
        self.input_hashes = input_hashes;
        self.previous_id = previous_id;
        self.skip = skip;
        self.request = Some(request);
    }
    fn finish_attempt(&mut self, outcome: WebSocketAttemptOutcome<'_>) {
        if let Some(mut observer) = self.attempt_observer.take() {
            observer.finish(outcome);
        }
        self.attempt_active = false;
    }

    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<SessionEvent>, WebSocketError> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            if self.prepare_request.is_some() && cancellation.is_cancelled() {
                self.done = true;
                return Err(WebSocketError::Cancelled);
            }
            self.started = true;
            if !self.session.http_only && self.request.is_none() {
                self.session.reset_stale_connection();
                match self.prepare_request.take().expect("lazy request factory")() {
                    Ok(request) => self.prepare(request),
                    Err(error) => {
                        self.done = true;
                        return Err(error);
                    }
                }
            }
        }
        if self.session.http_only {
            return self.next_http(cancellation).await;
        }
        let result = self.next_websocket(cancellation).await;
        match result {
            Ok(Some(event)) => {
                self.visible |= !matches!(event, ResponsesEvent::EncryptedReasoningReceived);
                if let ResponsesEvent::Completed(completion) = &event {
                    self.store_completion(completion);
                }
                Ok(Some(SessionEvent::Response(event)))
            }
            Ok(None) => {
                self.finish_attempt(WebSocketAttemptOutcome::Completed);
                if self.session.completed.is_none() {
                    self.session.poison();
                }
                self.done = true;
                Ok(None)
            }
            Err(error) => self.failure(error),
        }
    }
    async fn next_http(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<SessionEvent>, WebSocketError> {
        if self.http_events.is_none() {
            let turn_state = self.session.turn_state.clone().unwrap_or_default();
            let opened = if self.use_original_http {
                self.http.open_unprepared(turn_state, cancellation).await
            } else if let Some(request) = &self.request {
                self.http
                    .open(request.http_body().to_vec(), turn_state, cancellation)
                    .await
            } else {
                self.http.open_unprepared(turn_state, cancellation).await
            };
            match opened {
                Ok(events) => self.http_events = Some(events),
                Err(error) => {
                    self.done = true;
                    return Err(error);
                }
            }
        }
        let events = self.http_events.as_mut().expect("HTTP stream was opened");
        let result = events.next_event(cancellation).await;
        self.session.capture_turn_state(events.take_turn_state());
        match result {
            Ok(Some(event)) => Ok(Some(event)),
            Ok(None) => {
                self.done = true;
                Ok(None)
            }
            Err(error) => {
                self.done = true;
                Err(error)
            }
        }
    }
    async fn next_websocket(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<ResponsesEvent>, WebSocketError> {
        if self.exchange.is_none() {
            self.attempt_active = true;
            self.attempt_observer = self.observer.as_ref().map(|observer| observer.start());
            if !self
                .session
                .connection
                .as_ref()
                .is_some_and(|connection| connection.state() == SocketState::Open)
            {
                self.session.poison();
                let headers = self.session.headers.headers(cancellation).await?;
                let connection = self.session.client.connect(&headers, cancellation).await?;
                let state = connection
                    .response_headers()
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-turn-state"))
                    .map(|(_, value)| value.clone());
                self.session.capture_turn_state(state);
                self.session.connection = Some(connection);
            }
            let request = self
                .request
                .as_ref()
                .expect("prepared WebSocket request")
                .encode(
                    &self.previous_id,
                    self.skip,
                    self.session.turn_state.as_deref().unwrap_or_default(),
                );
            if let Some(observer) = &mut self.attempt_observer {
                observer.dump_request(&request);
            }
            self.exchange = Some(
                self.session
                    .connection
                    .as_mut()
                    .expect("connected")
                    .begin_observed(
                        request,
                        &self.session.provider_id,
                        self.attempt_observer.as_deref_mut(),
                    )?,
            );
        }
        let state = self.exchange.as_mut().expect("exchange started");
        let result = self
            .session
            .connection
            .as_mut()
            .expect("connected")
            .next_observed(state, cancellation, &mut self.attempt_observer)
            .await;
        self.session.capture_turn_state(state.turn_state.take());
        result
    }
    fn failure(&mut self, error: WebSocketError) -> Result<Option<SessionEvent>, WebSocketError> {
        self.finish_attempt(WebSocketAttemptOutcome::Failed(&error));
        let full_reason = match &error {
            WebSocketError::Response(ResponsesError::ProviderResponse { error_code, .. })
                if !self.recovered && !self.visible =>
            {
                match error_code.as_str() {
                    "previous_response_not_found" => Some(
                        "Previous provider response is unavailable. Retrying the full request on a new WebSocket connection.",
                    ),
                    "websocket_connection_limit_reached" => Some(
                        "WebSocket connection limit reached. Retrying the full request on a new connection.",
                    ),
                    _ => None,
                }
            }
            _ => None,
        };
        let http_reason = match &error {
            WebSocketError::Upgrade { status, .. } if matches!(status, 404 | 405 | 426) => Some(
                format!("WebSocket upgrade is unsupported (HTTP {status}). Retrying over HTTP."),
            ),
            WebSocketError::MessageTooLarge => Some(format!("{error} Retrying over HTTP.")),
            _ => None,
        };
        self.exchange = None;
        self.session.completed = None;
        self.session.poison();
        if let Some(reason) = http_reason {
            self.session.select_http();
            return Ok(Some(SessionEvent::Retry { attempt: 1, reason }));
        }
        if let Some(reason) = full_reason {
            self.recovered = true;
            self.previous_id.clear();
            self.skip = 0;
            return Ok(Some(SessionEvent::Retry {
                attempt: 1,
                reason: reason.into(),
            }));
        }
        if self.visible && error != WebSocketError::Cancelled {
            self.session.http_only = true;
        }
        self.done = true;
        Err(match error {
            error @ (WebSocketError::Socket { .. }
            | WebSocketError::Wire(_)
            | WebSocketError::InvalidOperation(_)
            | WebSocketError::Response(
                ResponsesError::WireProtocol(_)
                | ResponsesError::Json(_)
                | ResponsesError::InvalidInteger { .. },
            )) => WebSocketError::TransportFailure {
                message: "Responses WebSocket transport failed.".into(),
                cause: Box::new(error),
            },
            error => error,
        })
    }
    fn store_completion(&mut self, completion: &ResponsesCompletion) {
        self.session.completed =
            if matches!(completion.finish_reason.as_str(), "stop" | "tool_calls")
                && !completion.response_id.is_empty()
            {
                let mut items = self.input_hashes.clone();
                if let Some(content) = &completion.provider_content {
                    if let Ok(mut replay) =
                        serde_json::from_str::<HashMap<String, Box<RawValue>>>(content)
                    {
                        if let Some(output) = replay.remove("Output") {
                            if let Ok(output) =
                                serde_json::from_str::<Vec<Box<RawValue>>>(output.get())
                            {
                                items.extend(output.iter().map(|item| hash(item.get().as_bytes())));
                            }
                        }
                    }
                }
                Some(CompletedResponse {
                    settings: self.settings,
                    response_id: completion.response_id.clone(),
                    items,
                })
            } else {
                None
            };
    }
}
impl Drop for SessionCall<'_> {
    fn drop(&mut self) {
        let attempt_active = self.attempt_active;
        self.finish_attempt(WebSocketAttemptOutcome::Disposed);
        if attempt_active
            && !self.done
            && !self.session.http_only
            && !self.exchange.as_ref().is_some_and(|state| state.done)
        {
            self.session.completed = None;
            if let Some(connection) = &mut self.session.connection {
                connection.abort();
            }
            self.session.poison();
        }
    }
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
