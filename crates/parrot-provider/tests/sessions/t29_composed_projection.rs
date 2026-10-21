//! Input-only observations of delegated T29 assertions through actual providers.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket as ws,
    sessions::{
        Error, Event, EventStream, Message, Provider, ProviderSessions, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
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
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const CONTRACT: &str = "t30-t29-composed-source-v1";
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":1,"output_tokens":1},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]}}"#;
const HTTP_COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
const SENTINEL: &str = "private-sentinel";

#[derive(Deserialize)]
struct Input {
    contract: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    case_id: String,
    group: String,
    provider: String,
    #[serde(default)]
    header_timeout_ms: i64,
    #[serde(default)]
    disable_websocket: bool,
    #[serde(default)]
    fallback: bool,
    #[serde(default)]
    maximum_request_bytes: usize,
    #[serde(default)]
    upgrade_failure: bool,
    #[serde(default)]
    code: String,
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
struct Credentials;
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("test-key".into()) })
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
struct SocketRecord {
    sent: Mutex<Vec<Vec<u8>>>,
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
            let mut assembling = self.record.assembling.lock().unwrap();
            assembling.extend_from_slice(bytes);
            if end {
                self.record
                    .sent
                    .lock()
                    .unwrap()
                    .push(std::mem::take(&mut assembling));
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
                    "fixture frame exceeds receive buffer".into(),
                ));
            }
            bytes[..frame.len()].copy_from_slice(frame.as_bytes());
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
    Socket(Arc<SocketRecord>),
    Failure(ws::WebSocketError),
}
#[derive(Clone)]
struct ConnectRecord {
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
        _: &'a str,
        headers: &'a ws::Headers,
        timeout: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ConnectedSocket, ws::WebSocketError>> {
        self.calls.lock().unwrap().push(ConnectRecord {
            headers: headers.clone(),
            timeout,
        });
        let outcome = self.outcomes.lock().unwrap().pop_front();
        Box::pin(async move {
            match outcome {
                Some(Connection::Failure(error)) => Err(error),
                Some(Connection::Socket(record)) => Ok(ws::ConnectedSocket {
                    socket: Box::new(Socket {
                        record,
                        frames: VecDeque::from([COMPLETED.into()]),
                    }),
                    response_headers: Vec::new(),
                }),
                None => Err(ws::WebSocketError::InvalidOperation(
                    "unexpected fixture connection".into(),
                )),
            }
        })
    }
}
fn socket() -> (Arc<SocketRecord>, Connection) {
    let record = Arc::new(SocketRecord::default());
    (record.clone(), Connection::Socket(record))
}
struct Bytes {
    data: Vec<u8>,
    offset: usize,
}
impl ByteStream for Bytes {
    fn read<'a>(
        &'a mut self,
        bytes: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = bytes.len().min(self.data.len() - self.offset);
            bytes[..count].copy_from_slice(&self.data[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct Http {
    replies: Mutex<VecDeque<(u16, String)>>,
    calls: AtomicUsize,
}
impl Http {
    fn new(replies: Vec<(u16, String)>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            calls: AtomicUsize::new(0),
        })
    }
    fn completed(count: usize) -> Arc<Self> {
        Self::new((0..count).map(|_| (200, HTTP_COMPLETED.into())).collect())
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl HttpTransport for Http {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move {
            let (status, body) =
                reply.ok_or_else(|| HttpError::Provider("unexpected fixture HTTP send".into()))?;
            Ok(HttpResponse {
                status,
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(Bytes {
                    data: body.into_bytes(),
                    offset: 0,
                }),
            })
        })
    }
}
fn provider(
    case: &Case,
    disable: bool,
    timeout: i64,
    maximum: usize,
    connector: Arc<Connector>,
    http: Arc<Http>,
) -> Result<Arc<dyn Provider>, Error> {
    match case.provider.as_str() {
        "chatgpt" => {
            let mut options = ChatGptOptions::new(Arc::new(Credentials));
            options.disable_websocket = disable;
            options.header_timeout_ms = timeout;
            options.maximum_request_bytes = maximum;
            Ok(Arc::new(ChatGptProvider::new(options, http, connector)?))
        }
        "compatible" => {
            let mut options = CompatibleOptions::new(
                "configured",
                "https://example.test/v1",
                Arc::new(Credentials),
            );
            options.protocol = Protocol::Responses;
            options.disable_websocket = disable;
            options.header_timeout_ms = timeout;
            options.maximum_request_bytes = maximum;
            Ok(Arc::new(OpenAICompatibleProvider::new(
                options, http, connector,
            )?))
        }
        _ => Err(Error::InvalidOperation(
            "unknown composed probe provider".into(),
        )),
    }
}
fn request(text: &str) -> Request {
    Request {
        model: "model".into(),
        messages: vec![Message::user(text)],
        ..Request::default()
    }
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
fn sent(record: &SocketRecord) -> usize {
    record.sent.lock().unwrap().len()
}
fn session_id(headers: &ws::Headers) -> Option<&str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("session-id"))
        .map(|(_, value)| value.as_str())
}
struct Sleeper;
impl RetrySleeper for Sleeper {
    fn sleep<'a>(
        &'a self,
        _: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        })
    }
}
fn retrying(provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
    Arc::new(RetryingProvider::with_sleeper(
        provider,
        5,
        Arc::new(Sleeper),
    ))
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn diagnostic_event(event: &DiagnosticEvent) -> Value {
    json!({"category":event.category,"operation":event.operation,"severity":format!("{:?}",event.severity),"user_session_id":event.user_session_id,"agent_session_id":event.agent_session_id,"correlation_id":event.correlation_id,"request_id":event.request_id,"transport":event.transport,"provider_id":event.provider_id,"model_id":event.model_id,"tool_name":event.tool_name,"outcome":event.outcome,"duration_milliseconds":event.duration_milliseconds,"count":event.count,"request_bytes":event.request_bytes,"response_bytes":event.response_bytes,"close_status":event.close_status,"error_code":event.error_code})
}
fn native_error(error: &Error) -> Value {
    let mut sources = Vec::new();
    let mut next = std::error::Error::source(error);
    while let Some(source) = next {
        sources.push(source.to_string());
        next = source.source();
    }
    json!({"display":error.to_string(),"debug":format!("{error:?}"),"source_chain":sources})
}
async fn observe(case: &Case) -> Result<(Value, Value), Error> {
    match case.group.as_str() {
        "header" => {
            let (_, connection) = socket();
            let connector = Connector::new(vec![connection]);
            let p = provider(
                case,
                false,
                case.header_timeout_ms,
                64 << 20,
                connector.clone(),
                Http::completed(0),
            )?;
            let session = p.open_session()?;
            let events = call(session.as_ref(), request("hello")).await?;
            session.close().await?;
            let recorded = connector.calls.lock().unwrap()[0].clone();
            let timeout = recorded.timeout;
            Ok((
                json!({"completed":completed(&events),"connect_timeout_ms":timeout.map_or(0, |value| value.as_millis() as u64)}),
                json!({"connect_timeout_ms":timeout.map(|value|value.as_millis() as u64),"headers":recorded.headers,"semantic_zero_mapping":"None means disabled timeout; this is not raw argument identity with a CLR TimeSpan.Zero."}),
            ))
        }
        "request_limit" => {
            let (record, connection) = socket();
            let connector = Connector::new(if case.fallback {
                vec![Connection::Failure(ws::WebSocketError::Upgrade {
                    status: 404,
                    message: "missing".into(),
                })]
            } else {
                vec![connection]
            });
            let http = Http::completed(0);
            let p = provider(
                case,
                case.disable_websocket,
                60_000,
                case.maximum_request_bytes,
                connector,
                http.clone(),
            )?;
            let session = p.open_session()?;
            let (_, result) = drain(session.call(request("hello"))?).await;
            session.close().await?;
            let failure = result.err();
            let too_large = matches!(
                failure,
                Some(
                    Error::Http(HttpError::RequestTooLarge(_))
                        | Error::Socket(ws::WebSocketError::RequestTooLarge { .. })
                )
            );
            let contains = failure.as_ref().is_some_and(|error| {
                error
                    .to_string()
                    .contains(&format!("exceeds {} bytes", case.maximum_request_bytes))
            });
            Ok((
                json!({"request_too_large":too_large,"limit_message_contains_expected":contains,"http_calls":http.count(),"socket_sends":sent(&record)}),
                failure.as_ref().map_or(Value::Null, native_error),
            ))
        }
        "sessions" => {
            let (first_record, first_socket) = socket();
            let (second_record, second_socket) = socket();
            let connector = Connector::new(vec![first_socket, second_socket]);
            let p = provider(
                case,
                false,
                60_000,
                64 << 20,
                connector.clone(),
                Http::completed(0),
            )?;
            let first = p.open_session()?;
            let second = p.open_session()?;
            for (session, text) in [(&first, "one"), (&second, "two")] {
                call(
                    session.as_ref(),
                    Request {
                        model: "gpt-5.6-sol".into(),
                        max_output_tokens: 4096,
                        messages: vec![Message::user(text)],
                        ..Request::default()
                    },
                )
                .await?;
            }
            first.close().await?;
            second.close().await?;
            let calls = connector.calls.lock().unwrap();
            let first_id = calls.first().and_then(|value| session_id(&value.headers));
            let second_id = calls.get(1).and_then(|value| session_id(&value.headers));
            Ok((
                json!({"connector_calls":calls.len(),"session_ids_nonempty":first_id.is_some_and(|x|!x.is_empty())&&second_id.is_some_and(|x|!x.is_empty()),"session_ids_distinct":first_id!=second_id,"first_socket_sends":sent(&first_record),"second_socket_sends":sent(&second_record)}),
                json!({"session_ids":[first_id,second_id]}),
            ))
        }
        "exhausted" => {
            let (independent_record, independent_socket) = socket();
            let mut outcomes = (0..6)
                .map(|_| {
                    Connection::Failure(if case.upgrade_failure {
                        ws::WebSocketError::Upgrade {
                            status: 502,
                            message: SENTINEL.into(),
                        }
                    } else {
                        ws::WebSocketError::Transport(SENTINEL.into())
                    })
                })
                .collect::<Vec<_>>();
            outcomes.push(independent_socket);
            let connector = Connector::new(outcomes);
            let http = Http::completed(2);
            let p = retrying(provider(
                case,
                false,
                60_000,
                64 << 20,
                connector.clone(),
                http.clone(),
            )?);
            let failed = p.open_session()?;
            let independent = p.open_session()?;
            let events = call(failed.as_ref(), request("one")).await?;
            let retries = events
                .iter()
                .filter_map(|event| match event.base_event() {
                    Event::Retry {
                        attempt,
                        retry_after,
                        reason,
                    } => Some((*attempt, retry_after.as_millis() as u64, reason.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            call(failed.as_ref(), request("two")).await?;
            call(independent.as_ref(), request("three")).await?;
            failed.close().await?;
            independent.close().await?;
            Ok((
                json!({"retry_attempts":retries.iter().map(|x|x.0).collect::<Vec<_>>(),"retry_delays_ms":retries.iter().map(|x|x.1).collect::<Vec<_>>(),"last_retry_text":retries.last().map_or("",|x|x.2.as_str()),"connector_calls":connector.count(),"http_calls":http.count(),"independent_socket_sends":sent(&independent_record)}),
                Value::Null,
            ))
        }
        "fresh_budget" => {
            let connector = Connector::new(
                (0..6)
                    .map(|_| Connection::Failure(ws::WebSocketError::Transport("dropped".into())))
                    .collect(),
            );
            let http = Http::new(vec![
                (500, "transient".into()),
                (200, HTTP_COMPLETED.into()),
            ]);
            let p = retrying(provider(
                case,
                false,
                60_000,
                64 << 20,
                connector.clone(),
                http.clone(),
            )?);
            let session = p.open_session()?;
            let events = call(session.as_ref(), request("one")).await?;
            session.close().await?;
            Ok((
                json!({"connector_calls":connector.count(),"http_calls":http.count(),"completed":completed(&events)}),
                Value::Null,
            ))
        }
        "classifier" => {
            let failure = match case.code.as_str() {
                "websocket_closed" => ws::WebSocketError::Socket {
                    code: ws::SocketErrorCode::ConnectionClosedPrematurely,
                    message: SENTINEL.into(),
                },
                "websocket" => ws::WebSocketError::Socket {
                    code: ws::SocketErrorCode::Other,
                    message: SENTINEL.into(),
                },
                "wire_protocol" => ws::WebSocketError::Wire(SENTINEL.into()),
                "websocket_upgrade" => ws::WebSocketError::Upgrade {
                    status: 503,
                    message: SENTINEL.into(),
                },
                "websocket_transport" => ws::WebSocketError::Transport(SENTINEL.into()),
                "request_too_large" => ws::WebSocketError::RequestTooLarge { maximum: 1 },
                _ => {
                    return Err(Error::InvalidOperation(
                        "unknown transport classifier input".into(),
                    ));
                }
            };
            let injected = format!("{failure:?}");
            let p = provider(
                case,
                false,
                60_000,
                64 << 20,
                Connector::new(vec![Connection::Failure(failure)]),
                Http::completed(0),
            )?;
            let pool = ProviderSessions::new();
            let log = Arc::new(Log::default());
            let session = DiagnosticSession::new(
                pool.get(p.clone()).await?,
                log.clone(),
                Arc::new(MonotonicDiagnosticClock::default()),
                "websocket-agent".into(),
                p.id().into(),
                None,
            );
            let (_, result) = drain(session.call(request(SENTINEL))?).await;
            pool.close().await?;
            let error = result.err().ok_or_else(|| {
                Error::InvalidOperation("classifier produced no actual error".into())
            })?;
            let kind = match &error {
                Error::Socket(ws::WebSocketError::Upgrade { .. }) => "upgrade",
                Error::Socket(ws::WebSocketError::RequestTooLarge { .. }) => "request_too_large",
                Error::Socket(
                    ws::WebSocketError::TransportFailure { .. } | ws::WebSocketError::Transport(_),
                ) => "transport",
                _ => "unexpected",
            };
            if kind == "unexpected" {
                return Err(Error::InvalidOperation(format!(
                    "unrepresented actual classifier error: {error:?}"
                )));
            }
            let events = log.0.lock().unwrap();
            let snapshots = events.iter().map(diagnostic_event).collect::<Vec<_>>();
            let codes = events
                .iter()
                .filter_map(|event| event.error_code.clone())
                .collect::<Vec<_>>();
            let absent = !serde_json::to_string(&snapshots)
                .map_err(|error| Error::Encode(error.to_string()))?
                .contains(SENTINEL);
            let limitation = match case.code.as_str() {
                "websocket_upgrade" => {
                    "Upgrade retains status/message but has no field for the source IOException cause; no cause equivalence."
                }
                "request_too_large" => {
                    "RequestTooLarge carries maximum only, not the source private exception-message payload; absence is not same-payload runtime evidence."
                }
                _ => {
                    "Actual representable private message retained in typed injected error; diagnostics alone are checked for sentinel absence."
                }
            };
            Ok((
                json!({"error_kind":kind,"failure_codes":codes,"private_sentinel_absent":absent}),
                json!({"injected_native_variant":injected,"actual_error":native_error(&error),"diagnostic_events":snapshots,"representation_limit":limitation}),
            ))
        }
        _ => Err(Error::InvalidOperation(
            "unknown composed probe group".into(),
        )),
    }
}

pub fn recognizes(input: &str) -> Result<bool, serde_json::Error> {
    Ok(serde_json::from_str::<Value>(input)?["contract"] == CONTRACT)
}
pub async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Input = serde_json::from_str(input)?;
    if input.contract != CONTRACT {
        return Err("unexpected composed contract".into());
    }
    let mut cases = Vec::new();
    let mut native = Vec::new();
    for case in input.cases {
        let (observation, native_observation) = observe(&case).await?;
        cases.push(json!({"case_id":case.case_id,"group":case.group,"observation":observation}));
        native.push(json!({"case_id":case.case_id,"observation":native_observation,"construction_qualification":"Only the explicit canonical model/messages and case options are selected. Unasserted fields use Rust request/provider defaults; whole raw request/default/header identity with .NET is not claimed."}));
    }
    Ok(serde_json::to_string(
        &json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":CONTRACT,"cases":cases,"native_observations":native}),
    )?)
}
