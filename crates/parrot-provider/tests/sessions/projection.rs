//! Input-only finite probes of actual production composition. No oracle is read.
#[path = "auth_projection.rs"]
mod auth_projection;
#[path = "provider_consumer_projection.rs"]
mod provider_consumer_projection;
#[path = "review_projection.rs"]
mod review_projection;
#[path = "t29_composed_projection.rs"]
mod t29_composed_projection;
#[path = "t29_support_projection.rs"]
mod t29_support_projection;
#[path = "ws_auth_projection.rs"]
mod ws_auth_projection;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    http_stream::{HttpError, HttpFailure},
    responses::ResponsesError,
    responses_websocket::WebSocketError,
    sessions::{
        Completion, Content, Error, Event, EventStream, Message, Provider, ProviderSessions,
        ReasoningKind, Request, Session, ToolCall,
        decorators::{
            RetrySleeper, RetryingProvider, SubscriptionUsage, UsageReporter,
            UsageReportingProvider,
        },
        diagnostics::{DiagnosticEvent, DiagnosticSink, LastRequestDumper},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Deserialize)]
struct Inputs {
    cases: Vec<Case>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct Case {
    case_id: String,
    family: String,
    mode: String,
    failure: String,
    failures: usize,
    visible: String,
    fallback: bool,
    header_retries: u32,
    failure_sequence: Option<Vec<String>>,
    bodies: Vec<String>,
    protocol: String,
    keys: Vec<String>,
    maximum_request_bytes: usize,
    preferences: String,
    configured_auth: bool,
}
fn request() -> Request {
    Request {
        model: "synthetic-model".into(),
        messages: vec![Message::user("synthetic-input")],
        max_output_tokens: 500,
        ..Request::default()
    }
}
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
struct State {
    calls: usize,
    opens: usize,
    turns: usize,
    disposals: usize,
    enumerator_disposals: usize,
    fallback_calls: usize,
    credential_checks: usize,
    model_lists: usize,
    requests: Vec<i32>,
    trace: Vec<String>,
}
struct Script {
    input: Case,
    state: Arc<Mutex<State>>,
    reporter: Option<Arc<dyn UsageReporter>>,
}
impl Script {
    fn new(input: Case) -> Arc<Self> {
        Arc::new(Self {
            input,
            state: Arc::default(),
            reporter: None,
        })
    }
    fn stream(&self, request: Request) -> Box<dyn EventStream> {
        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        let call = state.calls;
        state.requests.push(request.max_output_tokens);
        state.trace.push(format!("call:{call}"));
        let failed = call
            <= self
                .input
                .failure_sequence
                .as_ref()
                .map_or(self.input.failures, Vec::len);
        let failure = self.input.failure_sequence.as_ref().map_or_else(
            || self.input.failure.clone(),
            |seq| seq.get(call - 1).cloned().unwrap_or_default(),
        );
        Box::new(ScriptStream {
            state: self.state.clone(),
            call,
            visible: self.input.visible.clone(),
            failure: failed.then(|| script_failure(&failure)),
            stage: 0,
            closed: false,
            diagnostic_mode: if self.input.family == "diagnostic" {
                self.input.mode.clone()
            } else {
                String::new()
            },
        })
    }
}
impl Provider for Script {
    fn id(&self) -> &str {
        "synthetic-provider"
    }
    fn usage_reporter(&self) -> Option<Arc<dyn UsageReporter>> {
        self.reporter.clone()
    }
    fn call(&self, r: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream(r))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async {
            self.state.lock().unwrap().credential_checks += 1;
            Ok(true)
        })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("synthetic-model", self.id())]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async {
            self.state.lock().unwrap().model_lists += 1;
            Ok(self.seed_models())
        })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.state.lock().unwrap().opens += 1;
        Ok(Arc::new(ScriptSession {
            script: Script {
                input: self.input.clone(),
                state: self.state.clone(),
                reporter: self.reporter.clone(),
            },
        }))
    }
}
struct ScriptSession {
    script: Script,
}
impl Session for ScriptSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.script.state.lock().unwrap().turns += 1;
            Ok(())
        })
    }
    fn call(&self, r: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.script.stream(r))
    }
    fn call_with_trace(
        &self,
        r: Request,
        trace: Arc<parrot_provider::sessions::diagnostics::CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(
            parrot_provider::sessions::diagnostic_session::RequestTraceStream::new(
                self.script.stream(r),
                trace,
                "synthetic".into(),
                "synthetic-request".into(),
            ),
        ))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async {
            let mut s = self.script.state.lock().unwrap();
            s.fallback_calls += 1;
            s.trace.push("fallback".into());
            Ok(self.script.input.fallback)
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.script.state.lock().unwrap().disposals += 1;
            if self.script.input.mode == "close-failure" {
                Err(Error::Io("synthetic session disposal failure".into()))
            } else {
                Ok(())
            }
        })
    }
}
struct ScriptStream {
    state: Arc<Mutex<State>>,
    call: usize,
    visible: String,
    failure: Option<Error>,
    stage: u8,
    closed: bool,
    diagnostic_mode: String,
}
impl ScriptStream {
    fn finish(&mut self) {
        if !self.closed {
            self.closed = true;
            let mut s = self.state.lock().unwrap();
            s.enumerator_disposals += 1;
            s.trace.push(format!("enumerator_dispose:{}", self.call));
        }
    }
}
impl Drop for ScriptStream {
    fn drop(&mut self) {
        self.finish();
    }
}
impl EventStream for ScriptStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if self.closed {
                return Ok(None);
            }
            if !self.diagnostic_mode.is_empty() {
                if self.stage == 0 {
                    self.stage = 1;
                    return Ok(Some(Event::Retry {
                        attempt: 2,
                        retry_after: Duration::from_millis(400),
                        reason: "synthetic retry".into(),
                    }));
                }
                if self.stage == 1 {
                    self.stage = 2;
                    if self.diagnostic_mode == "failure" {
                        self.finish();
                        return Err(Error::Wire("synthetic protocol failure".into()));
                    }
                    return Ok(Some(completed()));
                }
                self.finish();
                return if self.diagnostic_mode == "dispose-failure" {
                    Err(Error::Io("synthetic dispose failure".into()))
                } else {
                    Ok(None)
                };
            }
            if self.stage == 0 {
                self.stage = 1;
                if self.failure.is_some() && self.visible != "none" && !self.visible.is_empty() {
                    return Ok(Some(visible(&self.visible)));
                }
            }
            if let Some(error) = self.failure.take() {
                self.finish();
                return Err(error);
            }
            if self.stage < 2 {
                self.stage = 2;
                return Ok(Some(completed()));
            }
            self.finish();
            Ok(None)
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            let was_closed = self.closed;
            self.finish();
            if !was_closed && self.diagnostic_mode == "dispose-failure" {
                Err(Error::Io("synthetic dispose failure".into()))
            } else {
                Ok(())
            }
        })
    }
}
fn completed() -> Event {
    Event::Completed(Completion {
        finish_reason: "stop".into(),
        input_tokens: 11,
        cached_input_tokens: 3,
        output_tokens: 7,
        assistant_text: "synthetic answer".into(),
        ..Completion::default()
    })
}
fn visible(kind: &str) -> Event {
    (match kind {
        "TextDelta" => Event::TextDelta("synthetic fragment".into()),
        "ReasoningDelta" => Event::ReasoningDelta {
            text: "synthetic fragment".into(),
            kind: ReasoningKind::Raw,
            part_id: String::new(),
            completed: false,
        },
        "ToolCallDelta" => Event::ToolCallDelta(ToolCall::default()),
        "Completed" => Event::Completed(Completion::default()),
        "EncryptedReasoningReceived" => Event::EncryptedReasoningReceived,
        _ => panic!("unknown visible kind {kind}"),
    })
    .with_text("synthetic fragment")
}
fn script_failure(name: &str) -> Error {
    let response = |t: &str, c: &str, d: &str| {
        Error::Response(ResponsesError::ProviderResponse {
            error_type: t.into(),
            error_code: c.into(),
            detail: d.into(),
            response_body: String::new(),
        })
    };
    let http = |status, code: &str, detail: &str| {
        Error::Http(HttpError::Response(HttpFailure {
            status,
            error_type: String::new(),
            error_code: code.into(),
            detail: detail.into(),
            response_body: String::new(),
        }))
    };
    match name {
        "protocol" => Error::Wire("synthetic protocol failure".into()),
        "headers" => Error::HeaderTimeout("synthetic headers timeout".into()),
        "http503" => http(503, "", "synthetic unavailable"),
        "http429" => http(429, "", "synthetic rate limit"),
        "http400" => http(400, "", "synthetic bad request"),
        "overload-response" => response("engine_overloaded_error", "", "synthetic overloaded"),
        "usage" => response("insufficient_quota", "", "synthetic usage limit"),
        "context" => response("", "context_window_exceeded", "synthetic context limit"),
        "context-reduction" => http(
            400,
            "context_length_exceeded",
            "Requested token count exceeds the model's maximum context length of 1200 tokens. You requested a total of 1500 tokens: 1000 tokens from the input messages and 500 tokens for the completion.",
        ),
        "configuration" => Error::Provider("synthetic missing configuration".into()),
        "cancelled" => Error::Cancellation("synthetic cancellation".into()),
        "websocket" => Error::Socket(WebSocketError::Transport("synthetic websocket drop".into())),
        "upgrade429" => Error::Socket(WebSocketError::Upgrade {
            status: 429,
            message: "synthetic upgrade rejection".into(),
        }),
        _ => panic!("unknown failure {name}"),
    }
}
fn event(e: &Event) -> Value {
    let (kind, attempt, delay, input, cached, output) = match e.base_event() {
        Event::HttpRequestStarted => ("HttpRequestStarted", 0, 0, 0, 0, 0),
        Event::HttpResponseHeadersReceived => ("HttpResponseHeadersReceived", 0, 0, 0, 0, 0),
        Event::EncryptedReasoningReceived => ("EncryptedReasoningReceived", 0, 0, 0, 0, 0),
        Event::TextDelta(_) => ("TextDelta", 0, 0, 0, 0, 0),
        Event::ReasoningDelta { .. } => ("ReasoningDelta", 0, 0, 0, 0, 0),
        Event::ToolCallDelta(_) => ("ToolCallDelta", 0, 0, 0, 0, 0),
        Event::Retry {
            attempt,
            retry_after,
            ..
        } => ("Retry", *attempt, retry_after.as_millis() as u64, 0, 0, 0),
        Event::Completed(c) => (
            "Completed",
            0,
            0,
            c.input_tokens,
            c.cached_input_tokens,
            c.output_tokens,
        ),
        Event::WithText { .. } => unreachable!("base_event removes wrappers"),
    };
    json!({"kind":kind,"text":e.text(),"attempt":attempt,"delay_ms":delay,"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output})
}
fn error(e: &Error) -> Value {
    let kind = match e {
        Error::Http(HttpError::Response(_) | HttpError::Http(_) | HttpError::ProviderHttp(_)) => {
            "ProviderHttpException"
        }
        Error::Http(HttpError::RequestTooLarge(_)) => "ProviderRequestTooLargeException",
        Error::Http(HttpError::Provider(_)) => "LLMProviderException",
        Error::HeaderTimeout(_)
        | Error::Http(HttpError::HeaderTimeout(_))
        | Error::HeaderRetryLimit { .. } => "HeaderTimeoutException",
        Error::Response(ResponsesError::ProviderResponse { .. }) | Error::ChatProvider(_) => {
            "ProviderResponseException"
        }
        Error::Wire(_) => "WireProtocolException",
        Error::Cancellation(_) | Error::Cancelled | Error::Http(HttpError::Cancelled) => {
            "OperationCanceledException"
        }
        Error::Io(_) => "IOException",
        Error::InvalidOperation(_) => "InvalidOperationException",
        Error::Disposed => "ObjectDisposedException",
        Error::Provider(_) => "LLMProviderException",
        Error::Socket(WebSocketError::Transport(_)) => "ResponsesWebSocketTransportException",
        Error::Socket(WebSocketError::Upgrade { .. }) => "ResponsesWebSocketUpgradeException",
        _ => "RustUnmappedError",
    };
    json!({"type":kind,"message":e.to_string()})
}
async fn drain(mut stream: Box<dyn EventStream>) -> (Vec<Value>, Option<Value>, Vec<u64>) {
    let mut events = Vec::new();
    let mut delays = Vec::new();
    let mut failure = None;
    loop {
        let before = tokio::time::Instant::now();
        let result = stream.next(&Never).await;
        let elapsed = before.elapsed().as_millis() as u64;
        if elapsed > 0 {
            delays.push(elapsed)
        }
        match result {
            Ok(Some(e)) => events.push(event(&e)),
            Ok(None) => break,
            Err(e) => {
                failure = Some(error(&e));
                break;
            }
        }
    }
    if let Err(e) = stream.close().await {
        failure = Some(error(&e))
    }
    (events, failure, delays)
}
async fn retry(c: &Case) -> Value {
    let p = Script::new(c.clone());
    let sleeper = Arc::new(RecordingSleeper::default());
    let wrapper = RetryingProvider::with_sleeper(p.clone(), c.header_retries, sleeper.clone());
    let (events, failure, _elapsed) = if c.mode == "session" {
        let session = wrapper.open_session().unwrap();
        session.begin_turn().await.unwrap();
        let result = drain(session.call(request()).unwrap()).await;
        session.close().await.unwrap();
        result
    } else {
        drain(wrapper.call(request()).unwrap()).await
    };
    let s = p.state.lock().unwrap();
    json!({"events":events,"error":failure,"calls":s.calls,"opens":s.opens,"begin_turns":s.turns,"disposals":s.disposals,"enumerator_disposals":s.enumerator_disposals,"fallback_calls":s.fallback_calls,"requests_max_output_tokens":s.requests,"timer_delays_ms":*sleeper.0.lock().unwrap(),"trace":s.trace})
}
async fn pool(c: &Case) -> Value {
    let a = Script::new(Case {
        mode: c.mode.clone(),
        ..Case::default()
    });
    let b = Script::new(Case::default());
    let owner = ProviderSessions::new();
    let other = ProviderSessions::new();
    let first = owner.get(a.clone()).await.unwrap();
    let again = owner.get(a.clone()).await.unwrap();
    let isolated = other.get(a.clone()).await.unwrap();
    owner.begin_turn().await.unwrap();
    let late = owner.get(b.clone()).await.unwrap();
    owner.begin_turn().await.unwrap();
    drain(first.call(request()).unwrap()).await;
    drain(again.call(request()).unwrap()).await;
    drain(late.call(request()).unwrap()).await;
    let mut errors = Vec::new();
    for _ in 0..2 {
        errors.push(owner.close().await.err().map(|e| error(&e)))
    }
    let after = owner
        .get(a.clone())
        .await
        .err()
        .map(|e| error(&e)["type"].clone());
    let _ = other.close().await;
    let sa = a.state.lock().unwrap();
    let sb = b.state.lock().unwrap();
    json!({"same_reference":Arc::ptr_eq(&first,&again),"isolated_reference":!Arc::ptr_eq(&first,&isolated),"a_opens":sa.opens,"b_opens":sb.opens,"a_calls":sa.calls,"b_calls":sb.calls,"a_turns":sa.turns,"b_turns":sb.turns,"a_disposals":sa.disposals,"b_disposals":sb.disposals,"close_errors":errors,"get_after_close":after})
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, e: DiagnosticEvent) {
        self.0.lock().unwrap().push(e)
    }
}
fn diagnostic_event(e: &DiagnosticEvent) -> Value {
    json!({"category":e.category,"agent_session_id":e.agent_session_id,"provider_id":e.provider_id,"model_id":e.model_id,"operation":e.operation,"severity":format!("{:?}",e.severity),"outcome":e.outcome,"error_code":e.error_code,"count":e.count,"retry_delay_ms":if e.operation=="call_retry" {e.duration_milliseconds}else{None},"transport":e.transport,"request_bytes":e.request_bytes,"response_bytes":e.response_bytes,"close_status":e.close_status})
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut output, value| {
        write!(output, "{value:02x}").expect("writing a String cannot fail");
        output
    })
}
fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}
fn dump(c: &Case) -> Value {
    let directory = std::env::temp_dir().join(format!(
        "t30-rust-probe-{}-{}",
        std::process::id(),
        c.case_id
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("T30_SECRET_CANARY_latest");
    if c.mode == "failure-cleanup" {
        std::fs::create_dir(&path).unwrap()
    }
    let log = Arc::new(Log::default());
    let dumper = LastRequestDumper::new(&path, log.clone());
    let mut snapshots = Vec::new();
    let mut handles = Vec::new();
    for body in &c.bodies {
        let mut old = std::fs::File::open(&path)
            .ok()
            .filter(|f| f.metadata().unwrap().is_file());
        dumper.dump(&unhex(body));
        handles.push(old.as_mut().map(|f| {
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes).unwrap();
            hex(&bytes)
        }));
        snapshots.push(std::fs::read(&path).ok().map(|v| hex(&v)));
    }
    let result = json!({"snapshots_hex":snapshots,"replaced_open_handle_hex":handles,"temporary_file_exists":std::path::PathBuf::from(format!("{}.tmp",path.display())).is_file(),"diagnostics":log.0.lock().unwrap().iter().map(diagnostic_event).collect::<Vec<_>>()});
    std::fs::remove_dir_all(directory).unwrap();
    result
}
struct Reporter(Mutex<usize>);
impl UsageReporter for Reporter {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SubscriptionUsage, Error>> {
        Box::pin(async {
            *self.0.lock().unwrap() += 1;
            Ok(SubscriptionUsage {
                plan_type: "synthetic-subscription".into(),
                ..SubscriptionUsage::default()
            })
        })
    }
}
async fn usage() -> Value {
    let inner_reporter: Arc<dyn UsageReporter> = Arc::new(Reporter(Mutex::new(0)));
    let reporter = Arc::new(Reporter(Mutex::new(0)));
    let p = Arc::new(Script {
        input: Case::default(),
        state: Arc::default(),
        reporter: Some(inner_reporter.clone()),
    });
    let reporter_trait: Arc<dyn UsageReporter> = reporter.clone();
    let wrapper = UsageReportingProvider::new(p.clone(), reporter_trait.clone());
    let credential = wrapper.has_credential(&Never).await.unwrap();
    let models = wrapper.list_models(&Never).await.unwrap();
    let seed = wrapper.seed_models();
    let session = wrapper.open_session().unwrap();
    session.begin_turn().await.unwrap();
    let (events, failure, _) = drain(wrapper.call(request()).unwrap()).await;
    let exposed = wrapper.usage_reporter().unwrap();
    let quota = exposed.usage(&Never).await.unwrap();
    let result = {
        let s = p.state.lock().unwrap();
        json!({"id":wrapper.id(),"credential":credential,"list_count":models.len(),"seed_count":seed.len(),"reporter_same_reference":Arc::ptr_eq(&exposed,&reporter_trait),"reporter_is_inner":Arc::ptr_eq(&exposed,&inner_reporter),"plan_type":quota.plan_type,"reporter_calls":*reporter.0.lock().unwrap(),"credential_checks":s.credential_checks,"model_lists":s.model_lists,"opens":s.opens,"begin_turns":s.turns,"calls":s.calls,"events":events,"error":failure})
    };
    session.close().await.unwrap();
    result
}

pub async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    if t29_support_projection::recognizes(input)? {
        return t29_support_projection::project(input).await;
    }
    if provider_consumer_projection::recognizes(input)? {
        return provider_consumer_projection::project(input).await;
    }
    if t29_composed_projection::recognizes(input)? {
        return t29_composed_projection::project(input).await;
    }
    if ws_auth_projection::recognizes(input)? {
        return ws_auth_projection::project(input).await;
    }
    if auth_projection::recognizes(input)? {
        return auth_projection::project(input).await;
    }
    let parsed: Inputs = serde_json::from_str(input)?;
    if parsed
        .cases
        .iter()
        .any(|case| review_projection::is_review_family(&case.family))
    {
        return review_projection::project(input).await;
    }
    let input = parsed;
    let mut cases = Vec::new();
    for c in input.cases {
        let observation = match c.family.as_str() {
            "retry" => serde_json::to_string(&retry(&c).await)?,
            "sessions" => serde_json::to_string(&pool(&c).await)?,
            "dump" => serde_json::to_string(&dump(&c))?,
            "usage" => serde_json::to_string(&usage().await)?,
            "composition" => composition(&c).await?,
            "diagnostic" => serde_json::to_string(&diagnostic(&c).await)?,
            _ => return Err(format!("unknown probe family {}", c.family).into()),
        };
        cases.push(format!(
            "{{\"case_id\":{},\"family\":{},\"observation\":{}}}",
            serde_json::to_string(&c.case_id)?,
            serde_json::to_string(&c.family)?,
            observation
        ));
    }
    Ok(format!(
        "{{\"source_commit\":\"5f27a8a92625ee31d8bec094e5c3e09550193781\",\"contract\":\"t30-sessions-source-v1\",\"cases\":[{}]}}",
        cases.join(",")
    ))
}

use parrot_provider::{
    http_stream::{ByteStream, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers},
    sessions::{
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
struct Keys {
    values: Vec<String>,
    calls: Mutex<usize>,
}
impl ApiKeySource for Keys {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async {
            let mut calls = self.calls.lock().unwrap();
            let value = self.values[(*calls).min(self.values.len() - 1)].clone();
            *calls += 1;
            Ok(value)
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct Tokens {
    keys: Arc<Keys>,
    calls: Mutex<usize>,
}
impl OAuthTokenSource for Tokens {
    fn token<'a>(&'a self, c: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            *self.calls.lock().unwrap() += 1;
            Ok(OAuthAccess {
                access_token: self.keys.api_key(c).await?,
                account_id: "synthetic-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async {
            Err(WebSocketError::InvalidOperation(
                "unexpected WebSocket attempt in HTTP-only probe".into(),
            ))
        })
    }
}
struct Bytes(std::io::Cursor<Vec<u8>>);
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        b: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            std::io::Read::read(&mut self.0, b).map_err(|e| HttpError::Transport(e.to_string()))
        })
    }
}
#[derive(Serialize)]
struct HttpRecord {
    method: String,
    uri: String,
    headers: std::collections::BTreeMap<String, String>,
    body: Box<serde_json::value::RawValue>,
    body_bytes: usize,
}
struct Handler {
    protocol: String,
    requests: Mutex<Vec<HttpRecord>>,
    sessions: Mutex<Vec<String>>,
}
impl HttpTransport for Handler {
    fn send<'a>(
        &'a self,
        r: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let mut headers = std::collections::BTreeMap::new();
            for (name, mut value) in r.headers {
                let name = name.to_ascii_lowercase();
                if name == "content-type" {
                    continue;
                }
                if name == "x-synthetic-session" || name == "session-id" {
                    let mut ids = self.sessions.lock().unwrap();
                    let index = match ids.iter().position(|id| id == &value) {
                        Some(i) => i,
                        None => {
                            ids.push(value.clone());
                            ids.len() - 1
                        }
                    };
                    value = format!("session-{}", index + 1)
                }
                headers.insert(name, value);
            }
            self.requests.lock().unwrap().push(HttpRecord {
                method: format!("{:?}", r.method).to_ascii_uppercase(),
                uri: r.endpoint.as_url().to_string(),
                headers,
                body: serde_json::from_slice(&r.body)
                    .map_err(|e| HttpError::Wire(e.to_string()))?,
                body_bytes: r.body.len(),
            });
            let text = if self.protocol == "chat" {
                "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n"
            } else {
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n"
            };
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![],
                body: Box::new(Bytes(std::io::Cursor::new(text.as_bytes().to_vec()))),
            })
        })
    }
}
async fn composition(c: &Case) -> Result<String, Box<dyn std::error::Error>> {
    let keys = Arc::new(Keys {
        values: c.keys.clone(),
        calls: Mutex::new(0),
    });
    let oauth = Arc::new(Tokens {
        keys: keys.clone(),
        calls: Mutex::new(0),
    });
    let handler = Arc::new(Handler {
        protocol: c.protocol.clone(),
        requests: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
    });
    let provider: Result<Arc<dyn Provider>, Error> = if c.protocol == "chatgpt" {
        let mut options = ChatGptOptions::new(oauth.clone());
        options.maximum_request_bytes = c.maximum_request_bytes;
        ChatGptProvider::new(options, handler.clone(), Arc::new(NoSocket))
            .map(|p| Arc::new(p) as Arc<dyn Provider>)
    } else {
        let mut options = CompatibleOptions::new(
            "synthetic-provider",
            "https://provider.invalid/v1",
            keys.clone(),
        );
        options.protocol = if c.protocol == "responses" {
            Protocol::Responses
        } else {
            Protocol::ChatCompletions
        };
        options.session_header = "x-synthetic-session".into();
        options.maximum_request_bytes = c.maximum_request_bytes;
        options.provider_preferences = c.preferences.clone();
        options.headers = vec![
            ("User-Agent".into(), "synthetic-agent".into()),
            ("x-configured".into(), "synthetic-value".into()),
        ];
        if c.configured_auth {
            options.headers.push((
                "Authorization".into(),
                "synthetic-configured-ignored".into(),
            ))
        }
        OpenAICompatibleProvider::new(options, handler.clone(), Arc::new(NoSocket))
            .map(|p| Arc::new(p) as Arc<dyn Provider>)
    };
    let (provider_id, calls, construction_error) = match provider {
        Err(e) => (None, Vec::new(), Some(error(&e))),
        Ok(provider) => {
            let first = provider.open_session()?;
            let second = provider.open_session()?;
            let mut calls = Vec::new();
            for index in 0..c.keys.len() {
                let mut r = request();
                r.provider_preferences = "{\"ignored\":true}".into();
                let stream = if index < 2 {
                    first.call(r)?
                } else {
                    second.call(r)?
                };
                let (events, failure, _) = drain(stream).await;
                calls.push(json!({"events":events,"error":failure}));
            }
            first.close().await?;
            second.close().await?;
            (Some(provider.id().to_owned()), calls, None)
        }
    };
    #[derive(Serialize)]
    struct Output<'a> {
        provider_id: Option<String>,
        api_key_calls: usize,
        oauth_token_calls: usize,
        requests: &'a [HttpRecord],
        calls: Vec<Value>,
        construction_error: Option<Value>,
    }
    let records = handler.requests.lock().unwrap();
    Ok(serde_json::to_string(&Output {
        provider_id,
        api_key_calls: *keys.calls.lock().unwrap(),
        oauth_token_calls: *oauth.calls.lock().unwrap(),
        requests: &records,
        calls,
        construction_error,
    })?)
}

#[derive(Default)]
struct RecordingSleeper(Mutex<Vec<u64>>);
impl RetrySleeper for RecordingSleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if c.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.0.lock().unwrap().push(delay.as_millis() as u64);
            Ok(())
        })
    }
}
struct Clock;
impl parrot_provider::sessions::diagnostics::DiagnosticClock for Clock {
    fn now_millis(&self) -> u64 {
        0
    }
}
#[derive(Default)]
struct Cancel {
    value: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.value.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters()
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.value.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await
            }
        })
    }
}
async fn diagnostic(c: &Case) -> Value {
    let p = Script::new(c.clone());
    let log = Arc::new(Log::default());
    let session = parrot_provider::sessions::diagnostic_session::DiagnosticSession::new(
        p.open_session().unwrap(),
        log.clone(),
        Arc::new(Clock),
        "synthetic-agent".into(),
        "synthetic-provider".into(),
        None,
    );
    let cancel = Arc::new(Cancel::default());
    let mut stream = session.call_cancellable(request(), cancel.clone()).unwrap();
    let mut events = Vec::new();
    let mut failure = None;
    if c.mode == "early-disposal" || c.mode == "cancelled" {
        match stream.next(&Never).await {
            Ok(Some(e)) => events.push(event(&e)),
            Ok(None) => {}
            Err(e) => failure = Some(error(&e)),
        }
        if c.mode == "cancelled" {
            cancel.cancel();
            tokio::task::yield_now().await;
        }
    } else {
        loop {
            match stream.next(&Never).await {
                Ok(Some(e)) => events.push(event(&e)),
                Ok(None) => break,
                Err(e) => {
                    failure = Some(error(&e));
                    break;
                }
            }
        }
    }
    if let Err(e) = stream.close().await {
        failure = Some(error(&e))
    }
    let result = json!({"events":events,"error":failure,"enumerator_disposals":p.state.lock().unwrap().enumerator_disposals,"diagnostics":log.0.lock().unwrap().iter().map(diagnostic_event).collect::<Vec<_>>()});
    session.close().await.unwrap();
    result
}
