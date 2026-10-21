//! Actual transport observations from input-only scripts. ParseState's internal
//! response ID, output list and assistant-text accumulator are intentionally not
//! selected: T27 exposes completion/events, not the source parser's internals.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::responses::{ResponsesError, ResponsesEvent, ResponsesReasoningKind};
use parrot_provider::responses_websocket::{
    ConnectedSocket, Connector, FrameKind, Headers, ReceivedFrame, ResponsesWebSocket, Socket,
    SocketErrorCode, SocketState, WebSocketClient, WebSocketError, has_authority,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const INPUTS: &str = include_str!("fixtures/inputs.json");

#[derive(Clone)]
struct Token(Arc<(AtomicBool, tokio::sync::Notify)>);
impl Token {
    fn new(cancelled: bool) -> Self {
        Self(Arc::new((
            AtomicBool::new(cancelled),
            tokio::sync::Notify::new(),
        )))
    }
    fn cancel(&self) {
        self.0.0.store(true, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.0.1.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
fn number(input: &Value, key: &str, default: i64) -> i64 {
    input[key].as_i64().unwrap_or(default)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn headers_json(headers: &Headers) -> Value {
    Value::Object(
        headers
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}
fn state_name(state: SocketState) -> &'static str {
    match state {
        SocketState::Open => "Open",
        SocketState::Aborted => "Aborted",
        SocketState::Closed => "Closed",
        SocketState::Connecting => "Connecting",
    }
}

struct Frame {
    bytes: Vec<u8>,
    kind: FrameKind,
    end: bool,
    status: Option<u16>,
    description: Option<String>,
}
struct Recording {
    frames: VecDeque<Frame>,
    offset: usize,
    sent_frames: Vec<Value>,
    sent: Vec<u8>,
    receives: usize,
    aborted: bool,
    state: SocketState,
}
impl Recording {
    fn new(input: &Value) -> Self {
        let mut frames = VecDeque::new();
        for message in input["messages"].as_array().into_iter().flatten() {
            let mut bytes = message["text"].as_str().unwrap_or("").as_bytes().to_vec();
            if let Some(size) = message["pad_to"].as_u64() {
                bytes.resize(size as usize, b' ');
            }
            let kind = match message["type"].as_str().unwrap_or("Text") {
                "Text" => FrameKind::Text,
                "Binary" => FrameKind::Binary,
                "Close" => FrameKind::Close,
                other => panic!("unknown scripted frame {other}"),
            };
            let end = message["end"].as_bool().unwrap_or(true);
            for _ in 0..number(message, "repeat", 1) {
                let chunks = if let Some(split) = message["split_at"].as_u64() {
                    vec![
                        (&bytes[..split as usize], false),
                        (&bytes[split as usize..], end),
                    ]
                } else {
                    vec![(bytes.as_slice(), end)]
                };
                for (part, end) in chunks {
                    frames.push_back(Frame {
                        bytes: part.to_vec(),
                        kind,
                        end,
                        status: message["close_status"].as_u64().map(|n| n as u16),
                        description: message["close_description"].as_str().map(str::to_owned),
                    });
                }
            }
        }
        Self {
            frames,
            offset: 0,
            sent_frames: vec![],
            sent: vec![],
            receives: 0,
            aborted: false,
            state: SocketState::Open,
        }
    }
}
struct FakeSocket {
    recording: Arc<Mutex<Recording>>,
    stall_send: bool,
    exchange_tick: Option<i64>,
    tick: Arc<Mutex<i64>>,
}
async fn advance_to(tick: &Arc<Mutex<i64>>, target: i64) {
    let previous = *tick.lock().unwrap();
    assert!(target >= previous, "scripted time must be monotonic");
    tokio::time::advance(Duration::from_nanos((target - previous) as u64 * 100)).await;
    *tick.lock().unwrap() = target;
}
impl Socket for FakeSocket {
    fn state(&self) -> SocketState {
        self.recording.lock().unwrap().state
    }
    fn send<'a>(
        &'a mut self,
        bytes: &'a [u8],
        end: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            if self.stall_send {
                std::future::pending::<()>().await;
            }
            let mut recording = self.recording.lock().unwrap();
            recording.sent.extend_from_slice(bytes);
            recording
                .sent_frames
                .push(json!({"length":bytes.len(),"type":"Text","end":end}));
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        Box::pin(async move {
            self.recording.lock().unwrap().receives += 1;
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            if self.recording.lock().unwrap().frames.is_empty() {
                std::future::pending::<()>().await;
            }
            if let Some(target) = self.exchange_tick {
                advance_to(&self.tick, target).await;
            }
            let mut recording = self.recording.lock().unwrap();
            let frame = recording.frames.front().unwrap();
            let count = buffer.len().min(frame.bytes.len() - recording.offset);
            buffer[..count]
                .copy_from_slice(&frame.bytes[recording.offset..recording.offset + count]);
            let exhausted = recording.offset + count == frame.bytes.len();
            let result = ReceivedFrame {
                count,
                kind: frame.kind,
                end_of_message: exhausted && frame.end,
                close_status: frame.status,
                close_description: frame.description.clone(),
            };
            recording.offset += count;
            if exhausted {
                recording.frames.pop_front();
                recording.offset = 0;
            }
            Ok(result)
        })
    }
    fn abort(&mut self) {
        let mut record = self.recording.lock().unwrap();
        record.aborted = true;
        record.state = SocketState::Aborted;
    }
    fn dispose(&mut self) {
        self.recording.lock().unwrap().state = SocketState::Closed;
    }
}
fn socket(input: &Value, tick: Arc<Mutex<i64>>) -> (FakeSocket, Arc<Mutex<Recording>>) {
    let recording = Arc::new(Mutex::new(Recording::new(input)));
    (
        FakeSocket {
            recording: recording.clone(),
            stall_send: input["stall_send"] == true,
            exchange_tick: input["exchange_tick"].as_i64(),
            tick,
        },
        recording,
    )
}
async fn probes(
    input: &Value,
    key: &str,
    tick: &Arc<Mutex<i64>>,
    connection: &ResponsesWebSocket,
) -> Value {
    let mut result = vec![];
    for target in input[key].as_array().into_iter().flatten() {
        let target = target.as_i64().unwrap();
        advance_to(tick, target).await;
        result.push(json!({"tick":target,"stale":connection.stale()}));
    }
    json!(result)
}

pub fn observe_case(input: &Value) -> Value {
    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(async {
        match input["kind"].as_str().unwrap() {
            "send" => observe_send(input).await,
            "client" => observe_client(input).await,
            "authority" => json!({"case_id":input["case_id"],"has_authority":has_authority(input["endpoint"].as_str().unwrap_or(""),input["authority"].as_str().unwrap())}),
            kind => panic!("unknown fixture kind {kind}"),
        }
    })
}
async fn observe_send(input: &Value) -> Value {
    let tick = Arc::new(Mutex::new(number(input, "initial_tick", 0)));
    let (socket, recording) = socket(input, tick.clone());
    let mut connection = ResponsesWebSocket::new(
        Box::new(socket),
        vec![("x-synthetic-response".into(), "oracle".into())],
        Some(Duration::from_millis(
            number(input, "idle_timeout_ms", 1000) as u64,
        )),
        number(input, "maximum_request_bytes", i32::MAX as i64) as usize,
    );
    let request = input["request_text"]
        .as_str()
        .map(|text| text.as_bytes().to_vec())
        .unwrap_or_else(|| {
            (0..number(input, "request_bytes", 2) as usize)
                .map(|index| index as u8)
                .collect()
        });
    let mut result = json!({"case_id":input["case_id"],"request_size":request.len(),"request_sha256":hash(&request)});
    result["stale_before"] = probes(input, "stale_probe_ticks", &tick, &connection).await;
    let cancellation = Token::new(input["cancel_before"] == true);
    let timer = if number(input, "cancel_after_ms", 0) > 0 {
        let token = cancellation.clone();
        let duration = Duration::from_millis(number(input, "cancel_after_ms", 0) as u64);
        Some(tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            token.cancel();
        }))
    } else {
        None
    };
    let mut events = vec![];
    let mut failure = Value::Null;
    match connection.send(request, "oracle-provider") {
        Err(error) => failure = error_json(error, input, true),
        Ok(mut exchange) => loop {
            match exchange.next(&cancellation).await {
                Ok(Some(event)) => {
                    events.push(event_json(&event));
                    if events.len() as i64 == number(input, "cancel_after_events", -1) {
                        cancellation.cancel();
                    }
                    if events.len() as i64 == number(input, "stop_after_events", -1) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    failure = error_json(error, input, false);
                    break;
                }
            }
        },
    }
    if let Some(timer) = timer {
        timer.abort();
    }
    result["events"] = json!(events);
    result["exception"] = failure;
    result["request_bytes_recorded"] = json!(connection.stats().map(|s| s.request_bytes));
    result["response_bytes_recorded"] = json!(connection.stats().and_then(|s| s.response_bytes));
    result["close_status_recorded"] = json!(connection.stats().and_then(|s| s.close_status));
    result["response_headers"] = headers_json(connection.response_headers());
    result["socket_state_before_dispose"] = json!(state_name(connection.state()));
    {
        let r = recording.lock().unwrap();
        result["sent_frames"] = json!(r.sent_frames);
        result["sent_bytes"] = json!(r.sent.len());
        result["sent_sha256"] = json!(hash(&r.sent));
        result["receive_calls"] = json!(r.receives);
        result["remaining_scripted_frames"] = json!(r.frames.len());
        result["aborted_before_dispose"] = json!(r.aborted);
    }
    result["stale_after"] = probes(input, "after_probe_ticks", &tick, &connection).await;
    drop(connection);
    {
        let r = recording.lock().unwrap();
        result["aborted_after_dispose"] = json!(r.aborted);
        result["socket_state_after_dispose"] = json!(state_name(r.state));
    }
    result
}

#[derive(Default)]
struct ConnectRecord {
    calls: usize,
    endpoint: Option<String>,
    headers: Option<Headers>,
    timeout: Option<Duration>,
    token_matches: Option<bool>,
}
struct FakeConnector {
    record: Mutex<ConnectRecord>,
    socket: Mutex<Option<FakeSocket>>,
    failure: Option<String>,
    token: Token,
}
impl Connector for FakeConnector {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a Headers,
        timeout: Option<Duration>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async move {
            *self.record.lock().unwrap() = ConnectRecord {
                calls: 1,
                endpoint: Some(endpoint.into()),
                headers: Some(headers.clone()),
                timeout,
                token_matches: Some(std::ptr::eq(cancellation, &self.token as &dyn Cancellation)),
            };
            let error = match self.failure.as_deref() {
                Some("NotAWebSocket") => Some(WebSocketError::Socket {
                    code: SocketErrorCode::NotAWebSocket,
                    message: "scripted not a websocket".into(),
                }),
                Some("ConnectionClosedPrematurely") => Some(WebSocketError::Socket {
                    code: SocketErrorCode::ConnectionClosedPrematurely,
                    message: "scripted premature close".into(),
                }),
                Some("upgrade-429") => Some(WebSocketError::Upgrade {
                    status: 429,
                    message: "scripted upgrade".into(),
                }),
                Some("transport") => {
                    Some(WebSocketError::Transport("scripted connect timeout".into()))
                }
                Some("cancelled") => Some(WebSocketError::Cancelled),
                Some("invalid-operation") => Some(WebSocketError::InvalidOperation(
                    "scripted invalid operation".into(),
                )),
                None => None,
                Some(other) => panic!("unknown injected connector failure {other}"),
            };
            if let Some(error) = error {
                return Err(error);
            }
            Ok(ConnectedSocket {
                socket: Box::new(self.socket.lock().unwrap().take().unwrap()),
                response_headers: vec![
                    ("x-synthetic-response".into(), "oracle".into()),
                    ("x-request-id".into(), "synthetic-id".into()),
                ],
            })
        })
    }
}
async fn observe_client(input: &Value) -> Value {
    let (socket, recording) = socket(&Value::Null, Arc::new(Mutex::new(0)));
    let connector = Arc::new(FakeConnector {
        record: Mutex::new(ConnectRecord::default()),
        socket: Mutex::new(Some(socket)),
        failure: input["failure"].as_str().map(str::to_owned),
        token: Token::new(false),
    });
    let headers: Headers = input["headers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().into()))
        .collect();
    let endpoint = input["endpoint"].as_str().unwrap();
    let mut result = json!({"case_id":input["case_id"]});
    match WebSocketClient::endpoint(endpoint) {
        Ok(endpoint) => result["derived_endpoint"] = json!(endpoint),
        Err(error) => result["endpoint_exception"] = error_json(error, input, false),
    }
    let mut connection = None;
    let client = WebSocketClient::new(
        connector.clone(),
        endpoint,
        Some(Duration::from_millis(
            number(input, "connect_timeout_ms", 10000) as u64,
        )),
        Some(Duration::from_secs(1)),
        1024,
    );
    let connected = match client {
        Ok(client) => client.connect(&headers, &connector.token).await,
        Err(error) => Err(error),
    };
    match connected {
        Ok(value) => {
            result["response_headers"] = headers_json(value.response_headers());
            result["socket_state"] = json!(state_name(value.state()));
            result["stale"] = json!(value.stale());
            result["exception"] = Value::Null;
            connection = Some(value);
        }
        Err(error) => result["exception"] = error_json(error, input, false),
    }
    {
        let r = connector.record.lock().unwrap();
        result["connector_calls"] = json!(r.calls);
        result["connector_endpoint"] = json!(r.endpoint);
        result["connector_headers"] = r.headers.as_ref().map(headers_json).unwrap_or(Value::Null);
        result["connector_timeout_ticks"] =
            json!(r.timeout.map(|duration| duration.as_nanos() / 100));
        result["connector_token_matches_supplied"] = json!(r.token_matches);
    }
    result["caller_headers_after"] = headers_json(&headers);
    result["aborted_before_dispose"] = json!(recording.lock().unwrap().aborted);
    drop(connection);
    result["aborted_after_dispose"] = json!(recording.lock().unwrap().aborted);
    result
}

fn error_json(error: WebSocketError, input: &Value, _begin: bool) -> Value {
    let message = error.to_string();
    let kind = match error {
        WebSocketError::Cancelled => {
            // Rust has one cancellation category; source distinguishes an already
            // cancelled operation from an asynchronously cancelled task.
            let client = input["kind"] == "client";
            let synchronous = client
                || input["cancel_before"] == true
                || number(input, "cancel_after_events", -1) >= 0;
            return json!({"type":if synchronous {"System.OperationCanceledException"} else {"System.Threading.Tasks.TaskCanceledException"},"message":if synchronous {"The operation was canceled."} else {"A task was canceled."},"cancellation_requested":!client,"matches_supplied_token":client});
        }
        WebSocketError::Wire(_) => "Parrot.Llm.Wire.WireProtocolException",
        WebSocketError::RequestTooLarge { .. } => {
            "Parrot.Llm.Wire.ProviderRequestTooLargeException"
        }
        WebSocketError::MessageTooLarge => {
            "Parrot.Llm.Wire.ResponsesWebSocketMessageTooLargeException"
        }
        WebSocketError::Transport(_) | WebSocketError::TransportFailure { .. } => {
            "Parrot.Llm.Wire.ResponsesWebSocketTransportException"
        }
        WebSocketError::Upgrade { status, .. } => {
            return json!({"type":"Parrot.Llm.Wire.ResponsesWebSocketUpgradeException","message":message,"status_code":status});
        }
        WebSocketError::Socket { code, .. } => {
            return json!({"type":"System.Net.WebSockets.WebSocketException","message":message,"websocket_error_code":match code { SocketErrorCode::NotAWebSocket=>"NotAWebSocket",SocketErrorCode::ConnectionClosedPrematurely=>"ConnectionClosedPrematurely",SocketErrorCode::Faulted=>"Faulted",SocketErrorCode::Other=>"Other" }});
        }
        WebSocketError::InvalidOperation(_) => "System.InvalidOperationException",
        WebSocketError::InvalidRequest(_) => "Parrot.Llm.Wire.ProviderHttpException",
        WebSocketError::Disposed => "System.ObjectDisposedException",
        WebSocketError::Response(error) => return response_error(error),
    };
    json!({"type":kind,"message":message})
}
fn response_error(error: ResponsesError) -> Value {
    match error {
        ResponsesError::WireProtocol(message) => {
            json!({"type":"Parrot.Llm.Wire.WireProtocolException","message":message})
        }
        ResponsesError::ProviderResponse {
            error_type,
            error_code,
            detail,
            response_body,
        } => {
            let mut message = if detail.is_empty() {
                "provider error".to_owned()
            } else {
                detail.clone()
            };
            if !error_type.is_empty() {
                message.push_str(&format!(" ({error_type})"));
            }
            if !error_code.is_empty() {
                message.push_str(&format!(" [{error_code}]"));
            }
            json!({"type":"Parrot.Llm.Wire.ProviderResponseException","message":message,"error_type":error_type,"error_code":error_code,"detail":detail,"response_body":response_body})
        }
        ResponsesError::Json(message) => {
            let (_, location) = message
                .rsplit_once(" at line ")
                .expect("serde JSON location");
            let (line, column) = location.split_once(" column ").unwrap();
            let line = line.parse::<u64>().unwrap() - 1;
            let column = column.parse::<u64>().unwrap();
            json!({"type":"System.Text.Json.JsonReaderException","line_number":line,"byte_position_in_line":if message.starts_with("EOF ") {column} else {column.saturating_sub(1)}})
        }
        ResponsesError::InvalidInteger { .. } => {
            json!({"type":"Parrot.Llm.Wire.WireProtocolException","message":"responses: sequence_number must be an integer"})
        }
    }
}

fn event_json(event: &ResponsesEvent) -> Value {
    let mut result = json!({"Kind":"","Text":"","ReasoningKind":"Raw","ReasoningPartId":"","ReasoningCompleted":false,"ToolCallId":"","ToolName":"","Attempt":0,"RetryAfter":"00:00:00","FinishReason":"","InputTokens":0,"CachedInputTokens":0,"OutputTokens":0,"ToolCalls":[],"AssistantText":"","ProviderContent":{"ProviderId":"","Content":""},"IsVisibleOutput":true});
    match event {
        ResponsesEvent::TextDelta(text) => {
            result["Kind"] = json!("TextDelta");
            result["Text"] = json!(text);
        }
        ResponsesEvent::ReasoningDelta {
            text,
            kind,
            part_id,
            completed,
        } => {
            result["Kind"] = json!("ReasoningDelta");
            result["Text"] = json!(text);
            result["ReasoningKind"] = json!(if *kind == ResponsesReasoningKind::Summary {
                "Summary"
            } else {
                "Raw"
            });
            result["ReasoningPartId"] = json!(part_id);
            result["ReasoningCompleted"] = json!(completed);
        }
        ResponsesEvent::EncryptedReasoningReceived => {
            result["Kind"] = json!("EncryptedReasoningReceived");
            result["IsVisibleOutput"] = json!(false);
        }
        ResponsesEvent::ToolCallDelta(call) => {
            result["Kind"] = json!("ToolCallDelta");
            result["Text"] = json!(call.arguments_json);
            result["ToolCallId"] = json!(call.id);
            result["ToolName"] = json!(call.name);
        }
        ResponsesEvent::Completed(completion) => {
            result["Kind"] = json!("Completed");
            result["FinishReason"] = json!(completion.finish_reason);
            result["InputTokens"] = json!(completion.input_tokens);
            result["CachedInputTokens"] = json!(completion.cached_input_tokens);
            result["OutputTokens"] = json!(completion.output_tokens);
            result["AssistantText"] = json!(completion.assistant_text);
            result["ToolCalls"]=json!(completion.tool_calls.iter().map(|call|json!({"Id":call.id,"Name":call.name,"ArgumentsJson":call.arguments_json})).collect::<Vec<_>>());
            if let Some(content) = &completion.provider_content {
                result["ProviderContent"] =
                    json!({"ProviderId":completion.provider_id,"Content":content});
            }
        }
    }
    result
}
