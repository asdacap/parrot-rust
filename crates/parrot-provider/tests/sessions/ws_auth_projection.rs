//! Input-only actual-provider WS auth probes. Never reads source observations.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    catalogs::Model,
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Content, Error, Event, EventStream, Message, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::{CallTrace, DiagnosticClock, DiagnosticEvent, DiagnosticSink},
        http::Protocol,
        provider::{ApiKeySource, CompatibleOptions, OpenAICompatibleProvider},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const CONTRACT: &str = "t32-ws-auth-source-v1";
const DIAGNOSTICS_CONTRACT: &str = "t32-ws-auth-diagnostics-source-v1";
#[derive(Deserialize)]
struct Inputs {
    contract: String,
    cases: Vec<Case>,
    collaborators: Collaborators,
}
#[derive(Deserialize)]
struct Case {
    case_id: String,
    mode: String,
    disable_websocket: bool,
    key_script: String,
}
#[derive(Deserialize)]
struct Collaborators {
    source_commit: String,
    provider: ProviderInputs,
    request: RequestInputs,
    http_handler: HttpInputs,
}
#[derive(Deserialize)]
struct ProviderInputs {
    id: String,
    base_url: String,
    protocol: String,
}
#[derive(Deserialize)]
struct RequestInputs {
    model: String,
    messages: Vec<MessageInputs>,
}
#[derive(Deserialize)]
struct MessageInputs {
    role: String,
    text: String,
}
impl RequestInputs {
    fn build(&self) -> Result<Request, Error> {
        let messages = self
            .messages
            .iter()
            .map(|message| {
                if message.role == "user" {
                    Ok(Message::user(&message.text))
                } else {
                    Err(Error::InvalidOperation(
                        "unsupported public WS request role".into(),
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Request {
            model: self.model.clone(),
            messages,
            ..Request::default()
        })
    }
}
#[derive(Deserialize)]
struct HttpInputs {
    body: String,
}
pub(super) fn recognizes(input: &str) -> Result<bool, serde_json::Error> {
    #[derive(Deserialize)]
    struct Contract {
        contract: Option<String>,
    }
    let input: Contract = serde_json::from_str(input)?;
    Ok(matches!(
        input.contract.as_deref(),
        Some(CONTRACT | DIAGNOSTICS_CONTRACT)
    ))
}
#[derive(Default)]
struct Caller {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Caller {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Caller {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
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
#[derive(Default)]
struct Counts {
    physical_calls: AtomicUsize,
    enumerator_disposals: AtomicUsize,
    session_opens: AtomicUsize,
    session_disposals: AtomicUsize,
    begin_turns: AtomicUsize,
    fallback_calls: AtomicUsize,
    credential_gets: AtomicUsize,
    http_sends: AtomicUsize,
    connector_calls: AtomicUsize,
    attempt_errors: Mutex<Vec<Error>>,
    http_requests: Mutex<Vec<Value>>,
}
#[derive(Clone, Copy)]
enum KeyScript {
    AuthSixThenKey,
    AuthAlways,
    Missing,
    Cancel,
}
impl KeyScript {
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "auth_six_then_key" => Ok(Self::AuthSixThenKey),
            "auth_always" => Ok(Self::AuthAlways),
            "missing" => Ok(Self::Missing),
            "cancel" => Ok(Self::Cancel),
            _ => Err(Error::InvalidOperation(
                "unsupported public key script".into(),
            )),
        }
    }
}
struct Keys {
    script: KeyScript,
    counts: Arc<Counts>,
    caller: Arc<Caller>,
}
impl ApiKeySource for Keys {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            let call = self.counts.credential_gets.fetch_add(1, Ordering::SeqCst) + 1;
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            match self.script {
                KeyScript::AuthSixThenKey if call > 6 => Ok("synthetic-key".into()),
                KeyScript::AuthSixThenKey | KeyScript::AuthAlways => Err(Error::Authentication(
                    AuthError::Message("synthetic auth failure"),
                )),
                KeyScript::Missing => Ok(String::new()),
                KeyScript::Cancel => {
                    self.caller.cancel();
                    Err(Error::Cancellation("synthetic auth cancelled".into()))
                }
            }
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct Body(std::io::Cursor<Vec<u8>>);
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move { Ok(std::io::Read::read(&mut self.0, out).unwrap()) })
    }
}
struct Http {
    counts: Arc<Counts>,
    body: String,
}
impl HttpTransport for Http {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.counts.http_sends.fetch_add(1, Ordering::SeqCst);
            let authorization = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
                .map(|(_, value)| value.clone());
            self.counts.http_requests.lock().unwrap().push(json!({
                "method":match request.method { HttpMethod::Get => "GET", HttpMethod::Post => "POST" },
                "url":request.endpoint.as_str(), "authorization":authorization,
                "body_raw":String::from_utf8(request.body).expect("actual encoded HTTP body is UTF8"),
            }));
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![(
                    "Content-Type".into(),
                    "text/event-stream; charset=utf-8".into(),
                )],
                body: Box::new(Body(std::io::Cursor::new(self.body.as_bytes().to_vec()))),
            })
        })
    }
}
struct SocketConnector(Arc<Counts>);
impl Connector for SocketConnector {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async move {
            self.0.connector_calls.fetch_add(1, Ordering::SeqCst);
            panic!("synthetic connector must not be reached")
        })
    }
}
struct CountedProvider {
    inner: Arc<dyn Provider>,
    counts: Arc<Counts>,
}
impl Provider for CountedProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.inner.call(request)
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.inner.has_credential(cancellation)
    }
    fn seed_models(&self) -> Vec<Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        self.inner.list_models(cancellation)
    }
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        self.inner.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.counts.session_opens.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(CountedSession {
            inner: self.inner.open_session()?,
            counts: self.counts.clone(),
        }))
    }
}
struct CountedSession {
    inner: Arc<dyn Session>,
    counts: Arc<Counts>,
}
impl CountedSession {
    fn count(&self, stream: Box<dyn EventStream>) -> Box<dyn EventStream> {
        Box::new(CountedStream {
            inner: stream,
            counts: self.counts.clone(),
            finished: false,
        })
    }
}
impl Session for CountedSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.counts.begin_turns.fetch_add(1, Ordering::SeqCst);
        self.inner.begin_turn()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.counts.physical_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.count(self.inner.call(request)?))
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.counts.physical_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.count(self.inner.call_with_trace(request, trace)?))
    }
    fn call_with_retry_observation(
        &self,
        request: Request,
        observe: Arc<dyn Fn(i32, Duration) + Send + Sync>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.counts.physical_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.count(self.inner.call_with_retry_observation(request, observe)?))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.counts.fallback_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.counts.session_disposals.fetch_add(1, Ordering::SeqCst);
        self.inner.close()
    }
}
struct CountedStream {
    inner: Box<dyn EventStream>,
    counts: Arc<Counts>,
    finished: bool,
}
impl CountedStream {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.counts
                .enumerator_disposals
                .fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Drop for CountedStream {
    fn drop(&mut self) {
        self.finish();
    }
}
impl EventStream for CountedStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            let result = self.inner.next(cancellation).await;
            if let Err(error) = &result {
                self.counts
                    .attempt_errors
                    .lock()
                    .unwrap()
                    .push(error.clone());
            }
            if !matches!(result, Ok(Some(_))) {
                self.finish();
            }
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let result = self.inner.close().await;
            self.finish();
            result
        })
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
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
struct Clock;
impl DiagnosticClock for Clock {
    fn now_millis(&self) -> u64 {
        0
    }
}

/// Test-only finite source-type/ancestry counterparts, NOT a Rust generic API.
/// The WS source wrapper inherits IOException; real request diagnostics instead
/// recurse into its actual typed cause. No failure label or message is inspected.
fn counterpart(error: &Error) -> Result<(&'static str, &'static str), Error> {
    match error {
        Error::WebSocketTransport { .. } => {
            Ok(("Parrot.Llm.Wire.ResponsesWebSocketTransportException", "io"))
        }
        Error::Authentication(AuthError::Message(_)) => {
            Ok(("Parrot.Auth.AuthException", "unexpected"))
        }
        Error::Provider(_) => Ok(("Parrot.Llm.LLMProviderException", "unexpected")),
        Error::Cancelled | Error::Cancellation(_) => {
            Ok(("System.OperationCanceledException", "cancelled"))
        }
        Error::Io(_) => Ok(("System.IO.IOException", "io")),
        Error::InvalidOperation(_) => Ok(("System.InvalidOperationException", "invalid_operation")),
        _ => Err(Error::InvalidOperation(
            "actual WS runtime variant has no approved finite source counterpart".into(),
        )),
    }
}
fn native_error(error: &Error) -> Value {
    let mut chain = Vec::new();
    let mut next = std::error::Error::source(error);
    while let Some(cause) = next {
        let name = if cause.is::<Error>() {
            std::any::type_name::<Error>()
        } else if cause.is::<AuthError>() {
            std::any::type_name::<AuthError>()
        } else if cause.is::<HttpError>() {
            std::any::type_name::<HttpError>()
        } else if cause.is::<WebSocketError>() {
            std::any::type_name::<WebSocketError>()
        } else {
            std::any::type_name_of_val(cause)
        };
        chain.push(json!({"type":name,"variant":format!("{cause:?}"),"message":cause.to_string()}));
        next = cause.source();
    }
    json!({"type":std::any::type_name::<Error>(),"variant":format!("{error:?}"),"message":error.to_string(),"source_chain":chain})
}
// Replay only an already-observed typed error through the actual public
// DiagnosticSession to observe its request-context classifier. This side probe
// never participates in provider attempts, source counters or transport logic.
struct ErrorSession(Error);
impl Session for ErrorSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(ErrorStream(Some(self.0.clone()))))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct ErrorStream(Option<Error>);
impl EventStream for ErrorStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move { self.0.take().map_or(Ok(None), Err) })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
async fn request_classification(error: &Error) -> Result<String, Error> {
    let log = Arc::new(Log::default());
    let session = DiagnosticSession::new(
        Arc::new(ErrorSession(error.clone())),
        log.clone(),
        Arc::new(Clock),
        "ws-auth-category-probe".into(),
        "synthetic-provider".into(),
        None,
    );
    let mut stream = session.call(Request::default())?;
    let _ = stream.next(&Caller::default()).await;
    stream.close().await?;
    session.close().await?;
    let entries = log.0.lock().unwrap();
    entries
        .iter()
        .find(|event| event.operation == "call_finished")
        .and_then(|event| event.error_code.clone())
        .ok_or_else(|| {
            Error::InvalidOperation("actual diagnostic classifier emitted no terminal code".into())
        })
}
async fn project_error(error: &Error, dual: bool) -> Result<(Value, Value), Error> {
    let (name, generic) = counterpart(error)?;
    let request = request_classification(error).await?;
    let cause = if let Error::WebSocketTransport { cause, .. } = error {
        Box::pin(project_error(cause, dual)).await?.0
    } else {
        Value::Null
    };
    let selected = if dual {
        json!({"type":name,"message":error.to_string(),"generic_classification":generic,"request_classification":request,"cause":cause})
    } else {
        json!({"type":name,"message":error.to_string(),"classification":generic,"cause":cause})
    };
    let mut native = native_error(error);
    native["actual_request_classification"] = json!(request);
    native["test_only_source_generic_counterpart"] = json!(generic);
    Ok((selected, native))
}
fn event(event: &Event) -> Value {
    let (kind, attempt, delay) = match event.base_event() {
        Event::HttpRequestStarted => ("HttpRequestStarted", 0, 0),
        Event::HttpResponseHeadersReceived => ("HttpResponseHeadersReceived", 0, 0),
        Event::EncryptedReasoningReceived => ("EncryptedReasoningReceived", 0, 0),
        Event::TextDelta(_) => ("TextDelta", 0, 0),
        Event::ReasoningDelta { .. } => ("ReasoningDelta", 0, 0),
        Event::ToolCallDelta(_) => ("ToolCallDelta", 0, 0),
        Event::Retry {
            attempt,
            retry_after,
            ..
        } => ("Retry", *attempt, retry_after.as_millis() as u64),
        Event::Completed(_) => ("Completed", 0, 0),
        Event::WithText { .. } => unreachable!("base_event removes wrappers"),
    };
    json!({"kind":kind,"text":event.text(),"attempt":attempt,"delay_ms":delay})
}
fn diagnostic(event: &DiagnosticEvent) -> Value {
    json!({"category":event.category,"operation":event.operation,"severity":format!("{:?}",event.severity),"transport":event.transport,"outcome":event.outcome,"error_code":event.error_code,"count":event.count})
}
async fn run_case(
    case: &Case,
    collaborators: &Collaborators,
    dual: bool,
) -> Result<(Value, Value), Error> {
    if collaborators.provider.protocol != "Responses" {
        return Err(Error::InvalidOperation(
            "WS fixture requires Responses".into(),
        ));
    }
    let caller = Arc::new(Caller::default());
    let counts = Arc::new(Counts::default());
    let keys = Arc::new(Keys {
        script: KeyScript::parse(&case.key_script)?,
        counts: counts.clone(),
        caller: caller.clone(),
    });
    let mut options = CompatibleOptions::new(
        &collaborators.provider.id,
        &collaborators.provider.base_url,
        keys,
    );
    options.protocol = Protocol::Responses;
    options.disable_websocket = case.disable_websocket;
    let inner: Arc<dyn Provider> = Arc::new(OpenAICompatibleProvider::new(
        options,
        Arc::new(Http {
            counts: counts.clone(),
            body: collaborators.http_handler.body.clone(),
        }),
        Arc::new(SocketConnector(counts.clone())),
    )?);
    let counted: Arc<dyn Provider> = Arc::new(CountedProvider {
        inner,
        counts: counts.clone(),
    });
    let sleeper = Arc::new(Sleeper::default());
    let provider: Arc<dyn Provider> = match case.mode.as_str() {
        "retry_session" => Arc::new(RetryingProvider::with_sleeper(counted, 5, sleeper.clone())),
        "raw_session" => counted,
        _ => {
            return Err(Error::InvalidOperation(
                "unsupported public WS session mode".into(),
            ));
        }
    };
    let raw = provider.open_session()?;
    let log = Arc::new(Log::default());
    let diagnostic_session = dual.then(|| {
        DiagnosticSession::new(
            raw.clone(),
            log.clone(),
            Arc::new(Clock),
            "synthetic-agent-session".into(),
            collaborators.provider.id.clone(),
            None,
        )
    });
    let session: &dyn Session = diagnostic_session
        .as_ref()
        .map_or(raw.as_ref(), |session| session as &dyn Session);
    session.begin_turn().await?;
    let request = collaborators.request.build()?;
    let mut stream = if let Some(session) = &diagnostic_session {
        session.call_cancellable(request, caller.clone())?
    } else {
        session.call(request)?
    };
    let mut events = Vec::new();
    let mut terminal = None;
    loop {
        match stream.next(caller.as_ref()).await {
            Ok(Some(value)) => events.push(event(&value)),
            Ok(None) => break,
            Err(error) => {
                terminal = Some(error);
                break;
            }
        }
    }
    if let Err(error) = stream.close().await {
        terminal = Some(error);
    }
    session.close().await?;
    let mut attempt_errors = Vec::new();
    let mut native_attempt_errors = Vec::new();
    let attempts = counts.attempt_errors.lock().unwrap().clone();
    for error in &attempts {
        let (selected, native) = project_error(error, dual).await?;
        attempt_errors.push(selected);
        native_attempt_errors.push(native);
    }
    let (selected, native) = if let Some(error) = &terminal {
        project_error(error, dual).await?
    } else {
        (Value::Null, Value::Null)
    };
    let mut observation = json!({"events":events,"physical_calls":counts.physical_calls.load(Ordering::SeqCst),"enumerator_disposals":counts.enumerator_disposals.load(Ordering::SeqCst),"session_opens":counts.session_opens.load(Ordering::SeqCst),"session_disposals":counts.session_disposals.load(Ordering::SeqCst),"begin_turns":counts.begin_turns.load(Ordering::SeqCst),"fallback_calls":counts.fallback_calls.load(Ordering::SeqCst),"attempt_errors":attempt_errors,"delays_ms":*sleeper.0.lock().unwrap(),"error":selected,"caller_cancelled":caller.is_cancelled(),"credential_gets":counts.credential_gets.load(Ordering::SeqCst),"http_sends":counts.http_sends.load(Ordering::SeqCst),"connector_calls":counts.connector_calls.load(Ordering::SeqCst),"http_requests":*counts.http_requests.lock().unwrap()});
    let diagnostics = log
        .0
        .lock()
        .unwrap()
        .iter()
        .map(diagnostic)
        .collect::<Vec<_>>();
    if dual {
        observation["diagnostics"] = json!(diagnostics);
    }
    Ok((
        observation,
        json!({"case_id":case.case_id,"error":native,"attempt_errors":native_attempt_errors,"actual_session_diagnostics":diagnostics}),
    ))
}
pub(super) async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Inputs = serde_json::from_str(input)?;
    let dual = match input.contract.as_str() {
        CONTRACT => false,
        DIAGNOSTICS_CONTRACT => true,
        _ => return Err("unsupported public WS auth contract".into()),
    };
    let mut cases = Vec::new();
    let mut native_errors = Vec::new();
    for case in input.cases {
        let (observation, native) = run_case(&case, &input.collaborators, dual).await?;
        cases.push(json!({"case_id":case.case_id,"observation":observation}));
        native_errors.push(native);
    }
    Ok(serde_json::to_string(
        &json!({"source_commit":input.collaborators.source_commit,"contract":input.contract,"cases":cases,"native_errors":native_errors}),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn actual_variant_not_same_display_or_label_selects_generic_counterpart() {
        let failure_label = "auth_always";
        let bare = Error::Authentication(AuthError::Message("same display"));
        let wrapper = Error::WebSocketTransport {
            message: "same display".into(),
            cause: Box::new(bare.clone()),
        };
        assert_eq!(bare.to_string(), wrapper.to_string());
        assert_eq!(failure_label, "auth_always");
        assert_eq!(
            counterpart(&bare).unwrap(),
            ("Parrot.Auth.AuthException", "unexpected")
        );
        assert_eq!(
            counterpart(&wrapper).unwrap(),
            ("Parrot.Llm.Wire.ResponsesWebSocketTransportException", "io")
        );
        assert_eq!(request_classification(&bare).await.unwrap(), "unexpected");
        assert_eq!(
            request_classification(&wrapper).await.unwrap(),
            "unexpected"
        );
        let io = Error::Io("same display".into());
        assert_eq!(io.to_string(), bare.to_string());
        assert_eq!(counterpart(&io).unwrap(), ("System.IO.IOException", "io"));
        assert_eq!(request_classification(&io).await.unwrap(), "io");
        assert!(counterpart(&Error::Encode("same display".into())).is_err());
        assert!(
            counterpart(&Error::Socket(WebSocketError::Transport(
                "same display".into()
            )))
            .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn actual_request_classifier_recurses_typed_cause_not_wrapper_message() {
        for (cause, expected) in [
            (
                Error::Authentication(AuthError::Message("unchanged")),
                "unexpected",
            ),
            (Error::Io("unchanged".into()), "io"),
            (Error::Cancellation("unchanged".into()), "cancelled"),
            (
                Error::InvalidOperation("unchanged".into()),
                "invalid_operation",
            ),
        ] {
            let wrapper = Error::WebSocketTransport {
                message: "same outer display".into(),
                cause: Box::new(cause),
            };
            let (selected, native) = project_error(&wrapper, true).await.unwrap();
            assert_eq!(selected["generic_classification"], "io");
            assert_eq!(selected["request_classification"], expected);
            assert_eq!(native["actual_request_classification"], expected);
            assert_eq!(native["message"], "same outer display");
            assert_eq!(native["source_chain"][0]["message"], "unchanged");
        }
    }
}
