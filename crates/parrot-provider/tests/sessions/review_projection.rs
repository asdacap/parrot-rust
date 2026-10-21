//! Supplementary input-only probes over the actual T30 production boundaries.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Event, EventStream, Provider, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticClock, DiagnosticEvent, DiagnosticSink},
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Deserialize)]
struct Inputs {
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    case_id: String,
    family: String,
    protocol: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    credential: String,
}
#[derive(Default)]
struct Cancel {
    value: AtomicBool,
    changed: tokio::sync::Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.value.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.value.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
struct Credentials {
    mode: String,
    calls: AtomicUsize,
    released: AtomicBool,
    entered: tokio::sync::Notify,
    changed: tokio::sync::Notify,
}
impl Credentials {
    fn new(mode: &str) -> Arc<Self> {
        Arc::new(Self {
            mode: mode.into(),
            calls: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            changed: tokio::sync::Notify::new(),
        })
    }
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
    async fn resolve(&self, cancellation: &dyn Cancellation) -> Result<String, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_waiters();
        match self.mode.as_str() {
            "cancelled" => Err(Error::Cancellation(
                "synthetic credential cancellation".into(),
            )),
            "empty" => Ok(String::new()),
            "gated" => loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.released.load(Ordering::SeqCst) {
                    return Ok("synthetic-key".into());
                }
                tokio::select! {_ = cancellation.cancelled()=>return Err(Error::Cancelled),_ = changed=>{}}
            },
            _ => Ok("synthetic-key".into()),
        }
    }
}
impl ApiKeySource for Credentials {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(self.resolve(cancellation))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            Ok(OAuthAccess {
                access_token: self.resolve(cancellation).await?,
                account_id: "synthetic-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct Socket {
    calls: AtomicUsize,
}
impl Connector for Socket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(WebSocketError::InvalidOperation(
                "synthetic unexpected connector invocation".into(),
            ))
        })
    }
}
#[derive(Serialize)]
struct CapturedRequest {
    uri: String,
    body_raw: String,
}
struct Wire {
    protocol: String,
    outcome: String,
    calls: AtomicUsize,
    response_disposals: AtomicUsize,
    body_disposals: AtomicUsize,
    consumed: AtomicUsize,
    blocked: AtomicBool,
    requests: Mutex<Vec<CapturedRequest>>,
}
impl Wire {
    fn new(protocol: &str, outcome: &str) -> Arc<Self> {
        Arc::new(Self {
            protocol: protocol.into(),
            outcome: outcome.into(),
            calls: AtomicUsize::new(0),
            response_disposals: AtomicUsize::new(0),
            body_disposals: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
        })
    }
}
struct Transport(Arc<Wire>);
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let call = self.0.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.requests.lock().unwrap().push(CapturedRequest {
                uri: request.endpoint.as_str().into(),
                body_raw: String::from_utf8(request.body)
                    .map_err(|e| HttpError::Wire(e.to_string()))?,
            });
            let body = if self.0.outcome == "exhaust" || self.0.outcome == "recover" && call == 1 {
                "data: {broken\n\n"
            } else if self.0.protocol == "chat" {
                "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n"
            } else {
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n"
            };
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(Body {
                    wire: self.0.clone(),
                    cursor: std::io::Cursor::new(body.as_bytes().to_vec()),
                    supplied: false,
                }),
            })
        })
    }
}
struct Body {
    wire: Arc<Wire>,
    cursor: std::io::Cursor<Vec<u8>>,
    supplied: bool,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if self.wire.outcome == "partial" {
                if !self.supplied {
                    self.supplied = true;
                    buffer[..6].copy_from_slice(b"data: ");
                    self.wire.consumed.fetch_add(6, Ordering::SeqCst);
                    return Ok(6);
                }
                self.wire.blocked.store(true, Ordering::SeqCst);
                cancellation.cancelled().await;
                Err(HttpError::Cancelled)
            } else {
                std::io::Read::read(&mut self.cursor, buffer)
                    .map_err(|e| HttpError::Transport(e.to_string()))
            }
        })
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.wire.response_disposals.fetch_add(1, Ordering::SeqCst);
        self.wire.body_disposals.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Sleeper(Mutex<Vec<u64>>);
impl RetrySleeper for Sleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.0.lock().unwrap().push(delay.as_millis() as u64);
            Ok(())
        })
    }
}
fn provider(
    protocol: &str,
    credentials: Arc<Credentials>,
    wire: Arc<Wire>,
    socket: Arc<Socket>,
    websocket: bool,
) -> Result<Arc<dyn Provider>, Error> {
    let transport = Arc::new(Transport(wire));
    if protocol == "chatgpt" {
        let mut options = ChatGptOptions::new(credentials);
        options.disable_websocket = !websocket;
        ChatGptProvider::new(options, transport, socket).map(|p| Arc::new(p) as Arc<dyn Provider>)
    } else {
        let mut options = CompatibleOptions::new(
            "synthetic-provider",
            "https://provider.invalid/v1",
            credentials,
        );
        options.protocol = if protocol == "responses" {
            Protocol::Responses
        } else {
            Protocol::ChatCompletions
        };
        options.disable_websocket = !websocket;
        OpenAICompatibleProvider::new(options, transport, socket)
            .map(|p| Arc::new(p) as Arc<dyn Provider>)
    }
}
fn event(event: &Event) -> Value {
    let projected = super::event(event);
    json!({"kind":projected["kind"],"text":projected["text"],"attempt":projected["attempt"],"delay_ms":projected["delay_ms"]})
}
fn native_error(error: &Error) -> Value {
    json!({"type":std::any::type_name::<Error>(),"message":error.to_string(),"variant":format!("{error:?}")})
}
async fn drain(
    stream: &mut dyn EventStream,
    cancellation: &dyn Cancellation,
) -> (Vec<Value>, Option<Error>) {
    let mut events = Vec::new();
    let mut failure = None;
    loop {
        match stream.next(cancellation).await {
            Ok(Some(e)) => events.push(event(&e)),
            Ok(None) => break,
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    if let Err(error) = stream.close().await {
        failure = Some(error)
    }
    (events, failure)
}
async fn malformed(case: &Case) -> Result<(Value, Value), Error> {
    let credentials = Credentials::new("valid");
    let socket = Arc::new(Socket::default());
    let wire = Wire::new("chat", &case.outcome);
    let sleeper = Arc::new(Sleeper::default());
    let retry = RetryingProvider::with_sleeper(
        provider(
            "chat",
            credentials.clone(),
            wire.clone(),
            socket.clone(),
            false,
        )?,
        5,
        sleeper.clone(),
    );
    let session = if case.mode == "session" {
        Some(retry.open_session()?)
    } else {
        None
    };
    let mut stream = if let Some(session) = &session {
        session.call(super::request())?
    } else {
        retry.call(super::request())?
    };
    let (events, failure) = drain(stream.as_mut(), &Cancel::default()).await;
    if let Some(session) = session {
        session.close().await?
    }
    // Only this exact deliberately malformed Chat/SSE fixture has a parser-native
    // semantic projection. No other Encode/provider/domain error is normalized.
    let semantic_error = failure.as_ref().map(|error| match error {
        Error::Encode(_) => json!({"classification":"json_parse"}),
        _ => super::error(error),
    });
    let observation = json!({"events":events,"error":semantic_error,"credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"connect_calls":socket.calls.load(Ordering::SeqCst),"timer_delays_ms":*sleeper.0.lock().unwrap(),"requests":*wire.requests.lock().unwrap(),"response_disposals":wire.response_disposals.load(Ordering::SeqCst),"body_disposals":wire.body_disposals.load(Ordering::SeqCst)});
    Ok((
        observation,
        json!({"case_id":case.case_id,"error":failure.as_ref().map(native_error)}),
    ))
}
async fn auth(case: &Case) -> Result<Value, Error> {
    let credentials = Credentials::new(&case.credential);
    let socket = Arc::new(Socket::default());
    let wire = Wire::new(&case.protocol, "valid");
    let sleeper = Arc::new(Sleeper::default());
    let caller = Cancel::default();
    let retry = RetryingProvider::with_sleeper(
        provider(
            &case.protocol,
            credentials.clone(),
            wire.clone(),
            socket.clone(),
            true,
        )?,
        5,
        sleeper.clone(),
    );
    let session = retry.open_session()?;
    let mut stream = session.call(super::request())?;
    let (events, failure) = drain(stream.as_mut(), &caller).await;
    session.close().await?;
    Ok(
        json!({"events":events,"error":failure.as_ref().map(super::error),"credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"connect_calls":socket.calls.load(Ordering::SeqCst),"timer_delays_ms":*sleeper.0.lock().unwrap(),"requests":*wire.requests.lock().unwrap(),"response_disposals":wire.response_disposals.load(Ordering::SeqCst),"body_disposals":wire.body_disposals.load(Ordering::SeqCst),"caller_cancelled":caller.is_cancelled()}),
    )
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event)
    }
}
impl Log {
    fn project(&self) -> Vec<Value> {
        self.0.lock().unwrap().iter().map(|event|json!({"operation":event.operation,"severity":format!("{:?}",event.severity),"outcome":event.outcome,"error_code":event.error_code,"count":event.count,"retry_delay_ms":if event.operation=="call_retry"{event.duration_milliseconds}else{None},"transport":event.transport,"request_bytes":event.request_bytes,"response_bytes":event.response_bytes,"close_status":event.close_status})).collect()
    }
}
struct Clock;
impl DiagnosticClock for Clock {
    fn now_millis(&self) -> u64 {
        0
    }
}
async fn until_polled<T>(
    mut future: std::pin::Pin<&mut impl std::future::Future<Output = T>>,
    predicate: impl Fn() -> bool,
) {
    std::future::poll_fn(|context| {
        assert!(
            future.as_mut().poll(context).is_pending(),
            "probe expected an in-flight stage"
        );
        if predicate() {
            std::task::Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}
async fn partial(case: &Case) -> Result<Value, Error> {
    let credentials = Credentials::new("valid");
    let socket = Arc::new(Socket::default());
    let wire = Wire::new(&case.protocol, "partial");
    let log = Arc::new(Log::default());
    let provider = provider(&case.protocol, credentials, wire.clone(), socket, false)?;
    let session = DiagnosticSession::new(
        provider.open_session()?,
        log.clone(),
        Arc::new(Clock),
        "synthetic-agent".into(),
        provider.id().into(),
        None,
    );
    let caller = Arc::new(Cancel::default());
    let mut stream = session.call_cancellable(super::request(), caller.clone())?;
    let mut events = Vec::new();
    for _ in 0..2 {
        if let Some(e) = stream.next(caller.as_ref()).await? {
            events.push(event(&e))
        }
    }
    let pending_before_cancel;
    let consumed;
    let at_cancel;
    let mut failure = None;
    {
        let next = stream.next(caller.as_ref());
        tokio::pin!(next);
        until_polled(next.as_mut(), || wire.blocked.load(Ordering::SeqCst)).await;
        pending_before_cancel = true;
        consumed = wire.consumed.load(Ordering::SeqCst);
        caller.cancel();
        tokio::task::yield_now().await;
        at_cancel = log.project();
        match next.await {
            Ok(Some(e)) => events.push(event(&e)),
            Ok(None) => {}
            Err(e) => failure = Some(e),
        }
    }
    if let Err(e) = stream.close().await {
        failure = Some(e)
    }
    session.close().await?;
    let diagnostics = log.project();
    let request_terminals = diagnostics
        .iter()
        .filter(|e| e["operation"] == "request_finished")
        .count();
    let call_terminals = diagnostics
        .iter()
        .filter(|e| e["operation"] == "call_finished")
        .count();
    Ok(
        json!({"events":events,"error":failure.as_ref().map(super::error),"pending_before_cancel":pending_before_cancel,"consumed_response_bytes":consumed,"diagnostics_at_cancel":at_cancel,"diagnostics":diagnostics,"request_terminals":request_terminals,"call_terminals":call_terminals,"response_disposals":wire.response_disposals.load(Ordering::SeqCst),"body_disposals":wire.body_disposals.load(Ordering::SeqCst),"requests":*wire.requests.lock().unwrap()}),
    )
}
async fn pending(case: &Case) -> Result<Value, Error> {
    let credentials = Credentials::new("gated");
    let socket = Arc::new(Socket::default());
    let wire = Wire::new(&case.protocol, "valid");
    let provider = provider(
        &case.protocol,
        credentials.clone(),
        wire.clone(),
        socket,
        false,
    )?;
    let session = provider.open_session()?;
    let mut stream = if case.mode == "session" {
        session.call(super::request())?
    } else {
        provider.call(super::request())?
    };
    let caller = Cancel::default();
    let mut snapshots = Vec::new();
    {
        let next = stream.next(&caller);
        tokio::pin!(next);
        until_polled(next.as_mut(), || {
            credentials.calls.load(Ordering::SeqCst) > 0
        })
        .await;
        snapshots.push(json!({"phase":"credential_pending","credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"pull_completed":false}));
    }
    // Drop only the observation future; retain the owning stream and its stage.
    tokio::task::yield_now().await;
    snapshots.push(json!({"phase":"observer_absent","credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"pull_completed":false}));
    credentials.release();
    let mut events = Vec::new();
    let mut failure = None;
    match stream.next(&caller).await {
        Ok(Some(e)) => events.push(event(&e)),
        Ok(None) => {}
        Err(e) => failure = Some(e),
    }
    snapshots.push(json!({"phase":"same_pull_resumed","credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"pull_completed":true}));
    if failure.is_none() {
        let (rest, error) = drain(stream.as_mut(), &caller).await;
        events.extend(rest);
        failure = error
    } else {
        let _ = stream.close().await;
    }
    session.close().await?;
    Ok(
        json!({"events":events,"error":failure.as_ref().map(super::error),"snapshots":snapshots,"credential_calls":credentials.calls.load(Ordering::SeqCst),"http_calls":wire.calls.load(Ordering::SeqCst),"response_disposals":wire.response_disposals.load(Ordering::SeqCst),"body_disposals":wire.body_disposals.load(Ordering::SeqCst),"requests":*wire.requests.lock().unwrap()}),
    )
}
pub(super) fn is_review_family(family: &str) -> bool {
    matches!(
        family,
        "malformed_chat" | "websocket_auth" | "cancel_partial_http" | "pending_credentials"
    )
}
pub(super) async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Inputs = serde_json::from_str(input)?;
    let mut cases = Vec::new();
    let mut native_errors = Vec::new();
    for case in input.cases {
        let observation = match case.family.as_str() {
            "malformed_chat" => {
                let (observation, native) = malformed(&case).await?;
                native_errors.push(native);
                observation
            }
            "websocket_auth" => auth(&case).await?,
            "cancel_partial_http" => partial(&case).await?,
            "pending_credentials" => pending(&case).await?,
            _ => return Err(format!("unknown review probe family {}", case.family).into()),
        };
        cases.push(json!({"case_id":case.case_id,"family":case.family,"observation":observation}));
    }
    Ok(serde_json::to_string(
        &json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":"t30-sessions-review-source-v2","cases":cases,"native_errors":native_errors}),
    )?)
}
