//! Input-only observations of actual provider idle and continuation consumers.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Event, EventStream, Message, Provider, Request,
        diagnostics::{
            CallContext, CallTrace, DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock,
        },
        http::Protocol,
        provider::{
            ApiKeySource, CallTraceFactory, ChatGptOptions, ChatGptProvider, CompatibleOptions,
            OAuthAccess, OAuthTokenSource, OpenAICompatibleProvider,
        },
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
use tokio::sync::{Notify, Semaphore};

const CONTRACT: &str = "t30-provider-consumer-source-v1";
const RESPONSES: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"encrypted_content\":\"opaque-reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"summary\"}]}}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n";

#[derive(Deserialize)]
struct Input {
    contract: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    case_id: String,
    group: String,
    #[serde(default)]
    transport: String,
    #[serde(default)]
    fallback: bool,
    #[serde(default)]
    timeout_ms: i64,
    #[serde(default)]
    dialect: String,
    #[serde(default)]
    reject_continuation: bool,
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
struct Stop {
    stopped: AtomicBool,
    notify: Notify,
}
impl Stop {
    fn cancel(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Stop {
    fn is_cancelled(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            let waiting = self.notify.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            if !self.is_cancelled() {
                waiting.await;
            }
        })
    }
}
struct Credentials(&'static str);
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok(self.0.into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: "access-token".into(),
                account_id: "account-id".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct UnsupportedConnector {
    calls: AtomicUsize,
}
impl Connector for UnsupportedConnector {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(WebSocketError::Upgrade {
                status: 404,
                message: "missing".into(),
            })
        })
    }
}
struct BodyState {
    entered: Semaphore,
    disposed: AtomicBool,
}
impl Default for BodyState {
    fn default() -> Self {
        Self {
            entered: Semaphore::new(0),
            disposed: AtomicBool::new(false),
        }
    }
}
struct Body {
    payload: Option<Vec<u8>>,
    offset: usize,
    state: Arc<BodyState>,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.state.entered.add_permits(1);
            if let Some(payload) = &self.payload {
                let count = out.len().min(payload.len() - self.offset);
                out[..count].copy_from_slice(&payload[self.offset..self.offset + count]);
                self.offset += count;
                Ok(count)
            } else {
                cancellation.cancelled().await;
                Err(HttpError::Cancelled)
            }
        })
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.state.disposed.store(true, Ordering::SeqCst);
    }
}
struct Transport {
    requests: Mutex<Vec<HttpRequest>>,
    stalled: bool,
    reject: bool,
    state: Arc<BodyState>,
}
impl Transport {
    fn new(stalled: bool, reject: bool) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            stalled,
            reject,
            state: Arc::new(BodyState::default()),
        })
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let count = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len()
            };
            let rejected = self.reject && count > 1;
            let payload = (!self.stalled).then(|| {
                if rejected {
                    br#"{"error":{"code":"invalid_reasoning","message":"reasoning rejected"}}"#
                        .to_vec()
                } else {
                    RESPONSES.as_bytes().to_vec()
                }
            });
            Ok(HttpResponse {
                status: if rejected { 400 } else { 200 },
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(Body {
                    payload,
                    offset: 0,
                    state: self.state.clone(),
                }),
            })
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
struct TraceFactory {
    log: Arc<Log>,
}
impl CallTraceFactory for TraceFactory {
    fn start(&self, request: &Request) -> Arc<CallTrace> {
        Arc::new(CallTrace::start(
            self.log.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            CallContext {
                agent_session_id: "consumer-probe".into(),
                correlation_id: "consumer-call".into(),
                provider_id: "configured".into(),
                model_id: request.model.clone(),
            },
            None,
        ))
    }
}
fn provider(
    transport: &str,
    fallback: bool,
    idle: i64,
    http: Arc<Transport>,
    connector: Arc<UnsupportedConnector>,
    trace: Option<Arc<dyn CallTraceFactory>>,
) -> Result<Arc<dyn Provider>, Error> {
    if transport == "chatgpt" {
        let mut options = ChatGptOptions::new(Arc::new(Credentials("key")));
        options.disable_websocket = !fallback;
        options.stream_idle_timeout_ms = idle;
        options.trace_factory = trace;
        Ok(Arc::new(ChatGptProvider::new(options, http, connector)?))
    } else {
        let mut options = CompatibleOptions::new(
            if transport == "native" {
                "configured-provider"
            } else {
                "configured"
            },
            if transport == "native" {
                "https://provider.invalid/v1"
            } else {
                "https://example.test/v1"
            },
            Arc::new(Credentials(if transport == "native" {
                "test-key"
            } else {
                "key"
            })),
        );
        options.protocol = if transport == "chat-completions" {
            Protocol::ChatCompletions
        } else {
            Protocol::Responses
        };
        options.disable_websocket = !fallback;
        options.stream_idle_timeout_ms = idle;
        options.trace_factory = trace;
        Ok(Arc::new(OpenAICompatibleProvider::new(
            options, http, connector,
        )?))
    }
}
async fn consume(
    mut stream: Box<dyn EventStream>,
    cancellation: &dyn Cancellation,
) -> (Vec<Event>, Option<Error>) {
    let mut events = Vec::new();
    let error = loop {
        match stream.next(cancellation).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    let closed = stream.close().await.err();
    (events, error.or(closed))
}
fn event_kind(event: &Event) -> &'static str {
    match event.base_event() {
        Event::HttpRequestStarted => "HttpRequestStarted",
        Event::HttpResponseHeadersReceived => "HttpResponseHeadersReceived",
        Event::Retry { .. } => "Retry",
        _ => "Unexpected",
    }
}
fn diagnostic(event: &DiagnosticEvent) -> Value {
    json!({"category":event.category,"operation":event.operation,"severity":format!("{:?}",event.severity),"user_session_id":event.user_session_id,"agent_session_id":event.agent_session_id,"correlation_id":event.correlation_id,"request_id":event.request_id,"transport":event.transport,"provider_id":event.provider_id,"model_id":event.model_id,"tool_name":event.tool_name,"outcome":event.outcome,"duration_milliseconds":event.duration_milliseconds,"count":event.count,"request_bytes":event.request_bytes,"response_bytes":event.response_bytes,"close_status":event.close_status,"error_code":event.error_code})
}
async fn idle(case: &Case) -> Result<(Value, Value), Box<dyn std::error::Error>> {
    let http = Transport::new(true, false);
    let connector = Arc::new(UnsupportedConnector::default());
    let log = Arc::new(Log::default());
    let provider = provider(
        &case.transport,
        case.fallback,
        case.timeout_ms,
        http.clone(),
        connector.clone(),
        Some(Arc::new(TraceFactory { log: log.clone() })),
    )?;
    let session = provider.open_session()?;
    let request = Request {
        model: "model".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let stream = session.call(request)?;
    let stop = Arc::new(Stop::default());
    let task_stop = stop.clone();
    let task = tokio::spawn(async move { consume(stream, task_stop.as_ref()).await });
    tokio::time::timeout(Duration::from_secs(5), http.state.entered.acquire())
        .await??
        .forget();
    let (pending, sample_elapsed_ms) = if case.timeout_ms == 0 {
        let sample_started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let elapsed = sample_started.elapsed().as_secs_f64() * 1000.0;
        let pending = !task.is_finished();
        stop.cancel();
        (Some(pending), Some(elapsed))
    } else {
        (None, None)
    };
    let (events, error) = tokio::time::timeout(Duration::from_secs(5), task).await??;
    session.close().await?;
    let error = error.ok_or("stalled provider call unexpectedly completed")?;
    let error_kind = match &error {
        Error::Cancelled | Error::Http(HttpError::Cancelled) => "cancelled",
        Error::Wire(_) | Error::Http(HttpError::Wire(_)) => "wire_protocol",
        _ => "unexpected",
    };
    let entries = log.0.lock().unwrap().clone();
    let terminals: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == "request_finished" && e.transport.as_deref() == Some("http_sse"))
        .collect();
    if terminals.len() != 1 {
        return Err(format!(
            "expected one actual HTTP terminal, observed {}",
            terminals.len()
        )
        .into());
    }
    let terminal = terminals[0];
    let kinds: Vec<_> = events.iter().map(event_kind).collect();
    let observation = json!({"event_kinds":kinds,"events_allowed":kinds.iter().all(|kind|*kind != "Unexpected"),"error_kind":error_kind,"error_contains_idle":error.to_string().contains("idle"),"outcome":terminal.outcome,"response_bytes":terminal.response_bytes,"duration_nonnegative":terminal.duration_milliseconds.is_some(),"body_disposed":http.state.disposed.load(Ordering::SeqCst),"handler_calls":http.requests.lock().unwrap().len(),"connector_calls":connector.calls.load(Ordering::SeqCst),"pending_after_100ms":pending});
    Ok((
        observation,
        json!({"error_debug":format!("{error:?}"),"error_message":error.to_string(),"diagnostics":entries.iter().map(diagnostic).collect::<Vec<_>>(),"event_debug":events.iter().map(|e|format!("{e:?}")).collect::<Vec<_>>(),"pending_sample_elapsed_ms":sample_elapsed_ms,"measurement":"Real body-read signal, real 100 ms sleep, and JoinHandle::is_finished sampled before cancellation. Duration is actual unsigned monotonic metadata, not a fabricated numeric sample."}),
    ))
}
async fn native(case: &Case) -> Result<(Value, Value), Box<dyn std::error::Error>> {
    if case.dialect != "responses" {
        return Err("consumer observer only supports requested Responses tuples".into());
    }
    let http = Transport::new(false, case.reject_continuation);
    let provider = provider(
        "native",
        false,
        300_000,
        http.clone(),
        Arc::new(UnsupportedConnector::default()),
        None,
    )?;
    let session = provider.open_session()?;
    let initial = Request {
        model: "first-model".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let (events, first_error) = consume(session.call(initial.clone())?, &Never).await;
    if let Some(error) = first_error {
        return Err(error.into());
    }
    let completion = events
        .iter()
        .find_map(|event| match event.base_event() {
            Event::Completed(completion) => Some(completion.clone()),
            _ => None,
        })
        .ok_or("no actual completion")?;
    let provider_id_matches = completion.provider_id == provider.id();
    let assistant_text = completion.assistant_text.clone();
    let continued = Request {
        model: "second-model".into(),
        messages: vec![
            initial.messages[0].clone(),
            completion.into_message(),
            Message::user("next"),
        ],
        ..Request::default()
    };
    let (continuation, error) = consume(session.call(continued)?, &Never).await;
    session.close().await?;
    let continuation_error = match &error {
        None => "none",
        Some(Error::Http(HttpError::Response(_))) => "provider_http",
        _ => "unexpected",
    };
    let requests = http.requests.lock().unwrap();
    let encoded: Value =
        serde_json::from_slice(&requests.get(1).ok_or("second HTTP request absent")?.body)?;
    let input = encoded["input"]
        .as_array()
        .ok_or("actual encoded input array absent")?;
    let observation = json!({"provider_id_matches":provider_id_matches,"assistant_text":assistant_text,"continuation_error":continuation_error,"http_calls":requests.len(),"retry_count":continuation.iter().filter(|event|matches!(event.base_event(),Event::Retry{..})).count(),"include_encrypted_reasoning":encoded["include"].as_array().and_then(|values|values.first()).is_some_and(|value|value=="reasoning.encrypted_content"),"input_count":input.len(),"encrypted_content":input.get(1).and_then(|item|item.get("encrypted_content")),"reasoning_id":input.get(1).and_then(|item|item.get("id")),"continued_assistant_text":input.get(2).and_then(|item|item.get("content")).and_then(|content|content.get(0)).and_then(|item|item.get("text"))});
    Ok((
        observation,
        json!({"requests":requests.iter().map(|r|String::from_utf8_lossy(&r.body).into_owned()).collect::<Vec<_>>(),"continuation_error_debug":error.as_ref().map(|error|format!("{error:?}")),"continuation_error_message":error.as_ref().map(ToString::to_string),"initial_events":events.iter().map(|e|format!("{e:?}")).collect::<Vec<_>>(),"continuation_events":continuation.iter().map(|e|format!("{e:?}")).collect::<Vec<_>>()}),
    ))
}
pub fn recognizes(input: &str) -> Result<bool, serde_json::Error> {
    Ok(serde_json::from_str::<Value>(input)?["contract"] == CONTRACT)
}
pub async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Input = serde_json::from_str(input)?;
    if input.contract != CONTRACT {
        return Err("unexpected consumer contract".into());
    }
    // The registered producer pauses historical probes; these samples need real time.
    tokio::time::resume();
    let mut cases = Vec::new();
    let mut native_observations = Vec::new();
    for case in input.cases {
        let (observation, native) = match case.group.as_str() {
            "provider_idle" => idle(&case).await?,
            "native_http_reasoning" => native(&case).await?,
            _ => return Err("unknown provider consumer group".into()),
        };
        cases.push(json!({"case_id":case.case_id,"group":case.group,"observation":observation}));
        native_observations.push(json!({"case_id":case.case_id,"observation":native}));
    }
    Ok(json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":CONTRACT,"cases":cases,"native_observations":native_observations}).to_string())
}
