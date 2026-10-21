//! Actual provider observations for the remaining literal T29 predicates.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket as ws,
    sessions::{
        Error, Event, EventStream, Message, Provider, ProviderSessions, Request, Session,
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock},
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const CONTRACT: &str = "t30-t29-support-source-v1";
const COMPATIBLE_PHYSICAL_CONTRACT: &str = "t30-t29-compatible-physical-source-v1";
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]}}"#;
const HTTP_COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

#[derive(Deserialize)]
struct Input {
    contract: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Upgrade {
    status: u16,
    message: String,
}
#[derive(Deserialize)]
struct InputMessage {
    role: String,
    text: String,
}
#[derive(Deserialize)]
struct Case {
    case_id: String,
    group: String,
    provider: String,
    model: String,
    #[serde(default)]
    disable_websocket: bool,
    #[serde(default)]
    header_timeout_ms: i64,
    #[serde(default)]
    http_handler_delay_ms: u64,
    #[serde(default)]
    fallback: bool,
    #[serde(default)]
    upgrade_failure_if_fallback: Option<Upgrade>,
    #[serde(default)]
    fallback_upgrade_if_used: Option<Upgrade>,
    #[serde(default)]
    user_message: String,
    #[serde(default)]
    user_messages: Vec<String>,
    #[serde(default)]
    max_output_tokens: Option<i32>,
    #[serde(default)]
    behavior: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    first_user: String,
    #[serde(default)]
    second_user_if_reuse: String,
    #[serde(default)]
    completion_text: String,
    #[serde(default)]
    error_message_if_recovery: String,
    #[serde(default = "access_token")]
    access_token: String,
    #[serde(default = "account_id")]
    account_id: String,
    #[serde(default)]
    first_messages: Vec<InputMessage>,
    #[serde(default)]
    continued_messages: Vec<InputMessage>,
    #[serde(default)]
    response_ids: Vec<String>,
}
fn access_token() -> String {
    "access-token".into()
}
fn account_id() -> String {
    "account-id".into()
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
struct Credentials {
    token: String,
    account: String,
}
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("key".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: self.token.clone(),
                account_id: self.account.clone(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
#[derive(Default)]
struct SocketRecord {
    sent: Mutex<Vec<Vec<u8>>>,
    received: Mutex<Vec<Vec<u8>>>,
    assembling: Mutex<Vec<u8>>,
    aborted: AtomicBool,
}
struct Socket {
    record: Arc<SocketRecord>,
    frames: VecDeque<String>,
}
impl ws::Socket for Socket {
    fn state(&self) -> ws::SocketState {
        if self.record.aborted.load(Ordering::SeqCst) {
            ws::SocketState::Aborted
        } else {
            ws::SocketState::Open
        }
    }
    fn send<'a>(
        &'a mut self,
        bytes: &'a [u8],
        end: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), ws::WebSocketError>> {
        Box::pin(async move {
            let mut assembled = self.record.assembling.lock().unwrap();
            assembled.extend_from_slice(bytes);
            if end {
                self.record
                    .sent
                    .lock()
                    .unwrap()
                    .push(std::mem::take(&mut assembled));
            }
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ReceivedFrame, ws::WebSocketError>> {
        Box::pin(async move {
            let frame = self.frames.pop_front().ok_or_else(|| {
                ws::WebSocketError::InvalidOperation("unexpected fixture receive".into())
            })?;
            if frame.len() > bytes.len() {
                return Err(ws::WebSocketError::InvalidOperation(
                    "fixture frame exceeds buffer".into(),
                ));
            }
            bytes[..frame.len()].copy_from_slice(frame.as_bytes());
            self.record
                .received
                .lock()
                .unwrap()
                .push(frame.as_bytes().to_vec());
            Ok(ws::ReceivedFrame {
                count: frame.len(),
                kind: ws::FrameKind::Text,
                end_of_message: true,
                close_status: None,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {
        self.record.aborted.store(true, Ordering::SeqCst);
    }
}
enum Connection {
    Socket(Arc<SocketRecord>, Vec<String>),
    Failure(ws::WebSocketError),
}
#[derive(Clone)]
struct ConnectRecord {
    endpoint: String,
    headers: ws::Headers,
    timeout: Option<Duration>,
}
struct Connector {
    outcomes: Mutex<VecDeque<Connection>>,
    calls: Mutex<Vec<ConnectRecord>>,
}
impl Connector {
    fn new(outcomes: Vec<Connection>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            calls: Mutex::new(Vec::new()),
        })
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}
impl ws::Connector for Connector {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a ws::Headers,
        timeout: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ConnectedSocket, ws::WebSocketError>> {
        self.calls.lock().unwrap().push(ConnectRecord {
            endpoint: endpoint.into(),
            headers: headers.clone(),
            timeout,
        });
        let outcome = self.outcomes.lock().unwrap().pop_front();
        Box::pin(async move {
            match outcome {
                Some(Connection::Socket(record, frames)) => Ok(ws::ConnectedSocket {
                    socket: Box::new(Socket {
                        record,
                        frames: frames.into(),
                    }),
                    response_headers: Vec::new(),
                }),
                Some(Connection::Failure(error)) => Err(error),
                None => Err(ws::WebSocketError::InvalidOperation(
                    "unexpected fixture connection".into(),
                )),
            }
        })
    }
}
fn socket(frames: Vec<String>) -> (Arc<SocketRecord>, Connection) {
    let record = Arc::new(SocketRecord::default());
    (record.clone(), Connection::Socket(record, frames))
}
fn upgrade(value: &Upgrade) -> Connection {
    Connection::Failure(ws::WebSocketError::Upgrade {
        status: value.status,
        message: value.message.clone(),
    })
}
struct Bytes {
    offset: usize,
}
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let bytes = HTTP_COMPLETED.as_bytes();
            let count = out.len().min(bytes.len() - self.offset);
            out[..count].copy_from_slice(&bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct Http {
    requests: Mutex<Vec<HttpRequest>>,
    delay: Duration,
    allowed: bool,
}
impl Http {
    fn new(delay: Duration, allowed: bool) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            delay,
            allowed,
        })
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl HttpTransport for Http {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if !self.allowed {
                return Err(HttpError::Provider(
                    "unexpected fixture HTTP request".into(),
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {},
                () = cancellation.cancelled() => return Err(HttpError::Cancelled),
            }
            Ok(HttpResponse {
                status: 200,
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(Bytes { offset: 0 }),
            })
        })
    }
}
fn provider(
    case: &Case,
    connector: Arc<Connector>,
    http: Arc<Http>,
) -> Result<Arc<dyn Provider>, Error> {
    let credentials = Arc::new(Credentials {
        token: case.access_token.clone(),
        account: case.account_id.clone(),
    });
    let timeout = if case.group == "http_header" {
        case.header_timeout_ms
    } else {
        60_000
    };
    if case.provider == "chatgpt" {
        let mut options = ChatGptOptions::new(credentials);
        options.disable_websocket = case.disable_websocket;
        options.header_timeout_ms = timeout;
        Ok(Arc::new(ChatGptProvider::new(options, http, connector)?))
    } else if case.provider == "compatible" {
        let mut options =
            CompatibleOptions::new("configured", "https://example.test/v1", credentials);
        options.protocol = Protocol::Responses;
        options.disable_websocket = case.disable_websocket;
        options.header_timeout_ms = timeout;
        Ok(Arc::new(OpenAICompatibleProvider::new(
            options, http, connector,
        )?))
    } else {
        Err(Error::InvalidOperation(
            "unsupported support provider".into(),
        ))
    }
}
fn request(case: &Case, messages: Vec<Message>) -> Request {
    let mut request = Request {
        model: case.model.clone(),
        messages,
        ..Request::default()
    };
    if let Some(maximum) = case.max_output_tokens {
        request.max_output_tokens = maximum;
    }
    request
}
async fn drain(mut stream: Box<dyn EventStream>) -> (Vec<Event>, Result<(), Error>) {
    let mut events = Vec::new();
    let result = loop {
        match stream.next(&Never).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let closed = stream.close().await;
    (events, result.and(closed))
}
async fn call(session: &dyn Session, input: Request) -> Result<Vec<Event>, Error> {
    let (events, outcome) = drain(session.call(input)?).await;
    outcome?;
    Ok(events)
}
fn completed(events: &[Event]) -> bool {
    matches!(
        events.last().map(Event::base_event),
        Some(Event::Completed(_))
    )
}
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn diagnostic(event: &DiagnosticEvent) -> Value {
    json!({"category":event.category,"operation":event.operation,"severity":format!("{:?}",event.severity),"user_session_id":event.user_session_id,"agent_session_id":event.agent_session_id,"correlation_id":event.correlation_id,"request_id":event.request_id,"transport":event.transport,"provider_id":event.provider_id,"model_id":event.model_id,"tool_name":event.tool_name,"outcome":event.outcome,"duration_milliseconds":event.duration_milliseconds,"count":event.count,"request_bytes":event.request_bytes,"response_bytes":event.response_bytes,"close_status":event.close_status,"error_code":event.error_code})
}
fn native_http(http: &Http) -> Value {
    json!(http.requests.lock().unwrap().iter().map(|r| json!({"endpoint":r.endpoint.as_str(),"headers":r.headers,"body_bytes":r.body,"body_utf8":String::from_utf8(r.body.clone()).ok()})).collect::<Vec<_>>())
}
fn native_connects(connector: &Connector) -> Value {
    json!(connector.calls.lock().unwrap().iter().map(|c| json!({"endpoint":c.endpoint,"headers":c.headers,"timeout_ms":c.timeout.map(|d|d.as_millis())})).collect::<Vec<_>>())
}
fn physical_observation(entries: &[DiagnosticEvent], agent: &str) -> Value {
    let owned = entries
        .iter()
        .filter(|e| e.agent_session_id.as_deref() == Some(agent))
        .collect::<Vec<_>>();
    let starts = owned
        .iter()
        .copied()
        .filter(|e| e.operation == "request_started")
        .collect::<Vec<_>>();
    let finishes = owned
        .iter()
        .copied()
        .filter(|e| e.operation == "request_finished")
        .collect::<Vec<_>>();
    let call_starts = owned
        .iter()
        .copied()
        .filter(|e| e.operation == "call_started")
        .collect::<Vec<_>>();
    let call_finishes = owned
        .iter()
        .copied()
        .filter(|e| e.operation == "call_finished")
        .collect::<Vec<_>>();
    let all_call_starts = entries
        .iter()
        .filter(|e| e.operation == "call_started")
        .collect::<Vec<_>>();
    let sizes = |id: &Option<String>| {
        owned
            .iter()
            .copied()
            .filter(|e| e.operation == "request_size" && &e.request_id == id)
            .collect::<Vec<_>>()
    };
    let pair_count = starts.len() == finishes.len();
    let finish_ids_match = pair_count
        && starts
            .iter()
            .zip(&finishes)
            .all(|(a, b)| a.request_id == b.request_id);
    let finish_correlations_match = pair_count
        && starts
            .iter()
            .zip(&finishes)
            .all(|(a, b)| a.correlation_id == b.correlation_id);
    let size_correlations_match = starts.iter().all(|start| {
        sizes(&start.request_id)
            .iter()
            .all(|s| s.correlation_id == start.correlation_id)
    });
    let size_transports_match = starts.iter().all(|start| {
        sizes(&start.request_id)
            .iter()
            .all(|s| s.transport == start.transport)
    });
    let size_bytes_match_finish = pair_count
        && starts.iter().zip(&finishes).all(|(start, finish)| {
            sizes(&start.request_id)
                .iter()
                .all(|s| s.request_bytes == finish.request_bytes)
        });
    let missing_size_finish_request_bytes_null = pair_count
        && starts.iter().zip(&finishes).all(|(start, finish)| {
            !sizes(&start.request_id).is_empty() || finish.request_bytes.is_none()
        });
    json!({
        "call_starts":call_starts.len(),"call_finishes":call_finishes.len(),"request_starts":starts.len(),"request_finishes":finishes.len(),
        "call_correlations_unique":all_call_starts.iter().map(|e|&e.correlation_id).collect::<HashSet<_>>().len() == all_call_starts.len(),
        "request_ids_unique":starts.iter().map(|e|&e.request_id).collect::<HashSet<_>>().len() == starts.len(),
        "request_ids_nonempty":starts.iter().all(|e|e.request_id.as_ref().is_some_and(|s|!s.is_empty())),
        "finish_ids_match":finish_ids_match,"finish_correlations_match":finish_correlations_match,
        "single_call_match_per_request":starts.iter().all(|e|call_starts.iter().filter(|c|c.correlation_id == e.correlation_id).count() == 1),
        "start_transports":starts.iter().map(|e|&e.transport).collect::<Vec<_>>(),
        "finish_transports":finishes.iter().map(|e|&e.transport).collect::<Vec<_>>(),
        "finish_outcomes":finishes.iter().map(|e|&e.outcome).collect::<Vec<_>>(),
        "finish_durations_present":finishes.iter().all(|e|e.duration_milliseconds.is_some()),
        "size_event_at_most_one":starts.iter().all(|e|sizes(&e.request_id).len() <= 1),
        "size_correlations_match":size_correlations_match,"size_transports_match":size_transports_match,"size_bytes_match_finish":size_bytes_match_finish,
        "missing_size_finish_request_bytes_null":missing_size_finish_request_bytes_null,
        "request_entry_counts_match":starts.iter().all(|start|entries.iter().filter(|e|e.request_id == start.request_id).count() == 2+sizes(&start.request_id).len())
    })
}
async fn observe(case: &Case) -> Result<(Value, Value), Error> {
    match case.group.as_str() {
        "http_header" => {
            let connection = case
                .upgrade_failure_if_fallback
                .as_ref()
                .ok_or_else(|| Error::InvalidOperation("missing public upgrade input".into()))?;
            let connector = Connector::new(if case.fallback {
                vec![upgrade(connection)]
            } else {
                vec![]
            });
            let http = Http::new(Duration::from_millis(case.http_handler_delay_ms), true);
            let p = provider(case, connector.clone(), http.clone())?;
            let session = p.open_session()?;
            let started = Instant::now();
            let (events, outcome) =
                drain(session.call(request(case, vec![Message::user(&case.user_message)]))?).await;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            session.close().await?;
            let kind = match &outcome {
                Ok(()) => "none",
                Err(Error::Http(HttpError::HeaderTimeout(_)) | Error::HeaderTimeout(_)) => {
                    "header_timeout"
                }
                Err(error) => return Err(error.clone()),
            };
            Ok((
                json!({"completed":completed(&events),"error_kind":kind,"http_calls":http.count(),"connector_calls":connector.count()}),
                json!({"elapsed_wall_ms":elapsed,"error_debug":outcome.err().map(|e|format!("{e:?}")),"events":events.iter().map(|e|format!("{e:?}")).collect::<Vec<_>>(),"requests":native_http(&http),"connections":native_connects(&connector),"cause_qualification":"Upgrade enum preserves status/message but has no source IOException cause field."}),
            ))
        }
        "http_session" => {
            if case.user_messages.len() != 3 {
                return Err(Error::InvalidOperation(
                    "expected three public messages".into(),
                ));
            }
            let connector = Connector::new(vec![]);
            let http = Http::new(Duration::ZERO, true);
            let p = provider(case, connector.clone(), http.clone())?;
            let first = p.open_session()?;
            let second = p.open_session()?;
            for (session, text) in [
                (&first, &case.user_messages[0]),
                (&first, &case.user_messages[1]),
                (&second, &case.user_messages[2]),
            ] {
                call(session.as_ref(), request(case, vec![Message::user(text)])).await?;
            }
            first.close().await?;
            second.close().await?;
            let requests = http.requests.lock().unwrap();
            let ids = requests
                .iter()
                .map(|r| header(&r.headers, "session-id").map(str::to_owned))
                .collect::<Vec<_>>();
            if ids.len() != 3 {
                return Err(Error::InvalidOperation(
                    "unexpected actual HTTP request count".into(),
                ));
            }
            let observation = json!({"first_id_nonempty":ids[0].as_ref().is_some_and(|s|!s.is_empty()),"first_id_stable":ids[0] == ids[1],"sessions_distinct":ids[0] != ids[2],"http_calls":requests.len()});
            drop(requests);
            Ok((
                observation,
                json!({"session_ids":ids,"requests":native_http(&http),"connector_calls":connector.count()}),
            ))
        }
        "chatgpt_headers" => {
            if case.response_ids.len() != 2 {
                return Err(Error::InvalidOperation(
                    "expected two public response IDs".into(),
                ));
            }
            let (record, connection) = socket(
                case.response_ids
                    .iter()
                    .map(|id| COMPLETED.replace("resp-1", id))
                    .collect(),
            );
            let connector = Connector::new(vec![connection]);
            let http = Http::new(Duration::ZERO, false);
            let p = provider(case, connector.clone(), http.clone())?;
            let session = p.open_session()?;
            let mut actual_maxima = Vec::new();
            for messages in [&case.first_messages, &case.continued_messages] {
                let messages = messages
                    .iter()
                    .map(|m| match m.role.as_str() {
                        "user" => Ok(Message::user(&m.text)),
                        "assistant" => Ok(Message::assistant(&m.text, vec![])),
                        _ => Err(Error::InvalidOperation(
                            "unsupported public message role".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let input = request(case, messages);
                actual_maxima.push(input.max_output_tokens);
                call(session.as_ref(), input).await?;
            }
            session.close().await?;
            let calls = connector.calls.lock().unwrap();
            let headers = &calls
                .first()
                .ok_or_else(|| Error::InvalidOperation("no actual connection".into()))?
                .headers;
            let sent = record.sent.lock().unwrap();
            let first: Value = serde_json::from_slice(
                sent.first()
                    .ok_or_else(|| Error::InvalidOperation("no actual payload".into()))?,
            )
            .map_err(|e| Error::Encode(e.to_string()))?;
            Ok((
                json!({"authorization":header(headers,"Authorization").unwrap_or(""),"account_id":header(headers,"ChatGPT-Account-Id").unwrap_or(""),"max_output_tokens_present":first.get("max_output_tokens").is_some()}),
                json!({"connections":calls.iter().map(|c|json!({"endpoint":c.endpoint,"headers":c.headers,"timeout_ms":c.timeout.map(|d|d.as_millis())})).collect::<Vec<_>>(),"sent_utf8":sent.iter().map(|b|String::from_utf8(b.clone()).ok()).collect::<Vec<_>>(),"sent_bytes":*sent,"actual_request_max_output_tokens":actual_maxima,"http_calls":http.count()}),
            ))
        }
        "physical" => {
            let completed_frame = COMPLETED.replace("answer", &case.completion_text);
            let (recovered_record, recovered) =
                socket(vec![completed_frame.clone(), completed_frame]);
            let failed_frame = json!({"type":"error","error":{"code":case.behavior,"message":case.error_message_if_recovery}}).to_string();
            let (failed_record, failed) = socket(vec![failed_frame]);
            let outcomes = match case.behavior.as_str() {
                "reuse" => vec![recovered],
                "fallback" => vec![upgrade(case.fallback_upgrade_if_used.as_ref().ok_or_else(
                    || Error::InvalidOperation("missing public fallback input".into()),
                )?)],
                "previous_response_not_found" | "websocket_connection_limit_reached" => {
                    vec![failed, recovered]
                }
                _ => {
                    return Err(Error::InvalidOperation(
                        "unknown public physical behavior".into(),
                    ));
                }
            };
            let connector = Connector::new(outcomes);
            let http = Http::new(Duration::ZERO, true);
            let p = provider(case, connector.clone(), http.clone())?;
            let pool = ProviderSessions::new();
            let log = Arc::new(Log::default());
            let session = DiagnosticSession::new(
                pool.get(p.clone()).await?,
                log.clone(),
                Arc::new(MonotonicDiagnosticClock::default()),
                case.agent.clone(),
                p.id().into(),
                None,
            );
            let first = call(
                &session,
                request(case, vec![Message::user(&case.first_user)]),
            )
            .await?;
            if !completed(&first) {
                return Err(Error::InvalidOperation(
                    "physical call did not complete".into(),
                ));
            }
            if case.behavior == "reuse" {
                call(
                    &session,
                    request(case, vec![Message::user(&case.second_user_if_reuse)]),
                )
                .await?;
            }
            pool.close().await?;
            let entries = log.0.lock().unwrap().clone();
            let mut observation = physical_observation(&entries, &case.agent);
            let failed_sent = failed_record.sent.lock().unwrap();
            let recovered_sent = recovered_record.sent.lock().unwrap();
            let failed_received = failed_record.received.lock().unwrap();
            let recovered_received = recovered_record.received.lock().unwrap();
            let sent_frame_bytes = failed_sent
                .iter()
                .chain(recovered_sent.iter())
                .map(|bytes| Some(bytes.len() as u64))
                .collect::<Vec<_>>();
            let received_frame_bytes = failed_received
                .iter()
                .chain(recovered_received.iter())
                .map(|bytes| Some(bytes.len() as u64))
                .collect::<Vec<_>>();
            let finishes = entries
                .iter()
                .filter(|entry| {
                    entry.agent_session_id.as_deref() == Some(case.agent.as_str())
                        && entry.operation == "request_finished"
                })
                .collect::<Vec<_>>();
            let finish_request_bytes = finishes
                .iter()
                .map(|entry| entry.request_bytes)
                .collect::<Vec<_>>();
            let finish_response_bytes = finishes
                .iter()
                .map(|entry| entry.response_bytes)
                .collect::<Vec<_>>();
            observation["connector_calls"] = json!(connector.count());
            observation["http_calls"] = json!(http.count());
            observation["total_sent_count"] = json!(sent_frame_bytes.len());
            observation["request_bytes_match_sent"] = if case.behavior == "fallback" {
                Value::Null
            } else {
                json!(finish_request_bytes == sent_frame_bytes)
            };
            observation["response_bytes_match_received"] = if case.behavior == "fallback" {
                Value::Null
            } else {
                json!(finish_response_bytes == received_frame_bytes)
            };
            Ok((
                observation,
                json!({"diagnostics":entries.iter().map(diagnostic).collect::<Vec<_>>(),"connections":native_connects(&connector),"requests":native_http(&http),"recovered_sent_bytes":*recovered_sent,"failed_sent_bytes":*failed_sent,"recovered_received_bytes":*recovered_received,"failed_received_bytes":*failed_received,"sent_frame_bytes":sent_frame_bytes,"received_frame_bytes":received_frame_bytes,"finish_request_bytes":finish_request_bytes,"finish_response_bytes":finish_response_bytes,"source_cause_qualification":"Fallback Upgrade retains status/message but cannot carry source IOException(private-sentinel-exception) cause; no native cause equivalence."}),
            ))
        }
        _ => Err(Error::InvalidOperation("unknown support group".into())),
    }
}
pub fn recognizes(input: &str) -> Result<bool, serde_json::Error> {
    let contract = &serde_json::from_str::<Value>(input)?["contract"];
    Ok(contract == CONTRACT || contract == COMPATIBLE_PHYSICAL_CONTRACT)
}
pub async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Input = serde_json::from_str(input)?;
    if input.contract != CONTRACT && input.contract != COMPATIBLE_PHYSICAL_CONTRACT {
        return Err("unexpected support contract".into());
    }
    // The registered historical producer pauses time; these header acquisitions use real time.
    tokio::time::resume();
    let mut cases = Vec::new();
    let mut native = Vec::new();
    for case in input.cases {
        let (observation, details) = observe(&case).await?;
        cases.push(json!({"case_id":case.case_id,"group":case.group,"observation":observation}));
        native.push(json!({"case_id":case.case_id,"observation":details,"qualification":"Explicit original model/messages/options are constructed; unasserted whole-request/default/native identity and complete compiler closure are not claimed."}));
    }
    Ok(serde_json::to_string(
        &json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":input.contract,"cases":cases,"native_observations":native}),
    )?)
}
