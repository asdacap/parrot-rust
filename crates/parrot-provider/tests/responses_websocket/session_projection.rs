//! Input-only execution over the actual session and T27 adapters.
//! The test-only reference projection excludes CLR private fields/type identity,
//! T30 HTTP lifecycle events and T25 HTTP diagnostic/header production. All owned
//! response/retry values, request bytes, turn-state arguments, connector calls and
//! socket lifecycle counters are compared exactly.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::{ByteStream, HttpError};
use parrot_provider::responses::*;
use parrot_provider::responses_websocket::{
    ConnectedSocket, Connector as ConnectorTrait, FrameKind, HeaderSource, Headers,
    HttpEvents as HttpEventsTrait, HttpFallback, ReceivedFrame, Session, SessionEvent,
    Socket as SocketTrait, SocketState, WebSocketClient, WebSocketError, WebSocketRequest,
};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(test)]
const INPUTS: &str = include_str!("fixtures/session/accepted-v1/inputs.json");
#[cfg(test)]
const ORACLE: &str = include_str!("fixtures/session/accepted-v1/expected.json");
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn body(bytes: &[u8]) -> Value {
    let mut result = json!({"size_bytes": bytes.len(), "sha256": digest(bytes)});
    if bytes.len() <= 65536 {
        result["raw_json"] = String::from_utf8(bytes.to_vec()).unwrap().into();
    } else {
        let fields: serde_json::Map<String, Value> = serde_json::from_slice(bytes).unwrap();
        let raw: std::collections::HashMap<String, Box<RawValue>> =
            serde_json::from_slice(bytes).unwrap();
        let selected = raw
            .iter()
            .filter(|(key, _)| key.as_str() != "input")
            .map(|(key, value)| (key.clone(), Value::String(value.get().into())))
            .collect::<serde_json::Map<_, _>>();
        result["selected_raw_json_fields_excluding_input"] = selected.into();
        result["selected_input_structure"] = fields["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                item.as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| {
                        let value = if key != "content" {
                            Value::String(value.to_string())
                        } else {
                            value
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|content| {
                                    let mut parts = serde_json::Map::new();
                                    for (key, value) in content.as_object().unwrap() {
                                        if key == "text" {
                                            let text = value.as_str().unwrap().as_bytes();
                                            parts.insert(
                                                "text_utf8_bytes".into(),
                                                text.len().into(),
                                            );
                                            parts.insert(
                                                "text_utf8_sha256".into(),
                                                digest(text).into(),
                                            );
                                        } else {
                                            parts.insert(key.clone(), value.to_string().into());
                                        }
                                    }
                                    Value::Object(parts)
                                })
                                .collect::<Vec<_>>()
                                .into()
                        };
                        (key.clone(), value)
                    })
                    .collect::<serde_json::Map<_, _>>()
                    .into()
            })
            .collect::<Vec<Value>>()
            .into();
    }
    result
}

struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}
struct Empty;
impl ResponsesToolCallView for Empty {
    fn id(&self) -> &str {
        ""
    }
    fn name(&self) -> &str {
        ""
    }
    fn arguments_json(&self) -> &str {
        "{}"
    }
}
impl ResponsesToolDefinitionView for Empty {
    fn name(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_json(&self) -> &str {
        "{}"
    }
}

struct Content(String);
impl ResponsesContentView for Content {
    fn kind(&self) -> ResponsesContentKind {
        ResponsesContentKind::Text
    }
    fn text(&self) -> &str {
        &self.0
    }
    fn image_path(&self) -> &Path {
        Path::new("")
    }
    fn image_url(&self) -> &str {
        ""
    }
    fn image_media_type(&self) -> &str {
        ""
    }
    fn read_image(&self) -> Result<Vec<u8>, io::Error> {
        Ok(vec![])
    }
}
struct ToolCall(ResponsesToolCall);
impl ResponsesToolCallView for ToolCall {
    fn id(&self) -> &str {
        &self.0.id
    }
    fn name(&self) -> &str {
        &self.0.name
    }
    fn arguments_json(&self) -> &str {
        &self.0.arguments_json
    }
}
struct Message {
    role: ResponsesRole,
    content: [Content; 1],
    tool_call_id: String,
    tool_calls: Vec<ToolCall>,
    provider_id: String,
    provider_content: String,
}
impl Message {
    fn text(role: ResponsesRole, text: String) -> Self {
        Self {
            role,
            content: [Content(text)],
            tool_call_id: String::new(),
            tool_calls: vec![],
            provider_id: String::new(),
            provider_content: String::new(),
        }
    }
    fn from_input(input: &Value) -> Self {
        if let Some(text) = input.as_str() {
            return match text.strip_prefix("assistant:") {
                Some(text) => Self::text(ResponsesRole::Assistant, text.into()),
                None => Self::text(ResponsesRole::User, text.into()),
            };
        }
        let role = match input["role"].as_str().unwrap() {
            "user" => ResponsesRole::User,
            "assistant" => ResponsesRole::Assistant,
            "tool" => ResponsesRole::Tool,
            role => panic!("unknown message role {role}"),
        };
        let mut message = Self::text(role, input["text"].as_str().unwrap().into());
        message.tool_call_id = input["tool_call_id"].as_str().unwrap_or_default().into();
        message.tool_calls = input["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        ToolCall(ResponsesToolCall {
                            id: call["id"].as_str().unwrap().into(),
                            name: call["name"].as_str().unwrap().into(),
                            arguments_json: call["arguments"].as_str().unwrap().into(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        message.provider_id = input["provider_content"]["provider_id"]
            .as_str()
            .unwrap_or_default()
            .into();
        message.provider_content = input["provider_content"]["content"]
            .as_str()
            .unwrap_or_default()
            .into();
        message
    }
    fn from_completion(completion: &ResponsesCompletion) -> Self {
        let mut message = Self::text(ResponsesRole::Assistant, completion.assistant_text.clone());
        message.tool_calls = completion
            .tool_calls
            .iter()
            .cloned()
            .map(ToolCall)
            .collect();
        if let Some(content) = &completion.provider_content {
            message.provider_id = completion.provider_id.clone();
            message.provider_content = content.clone();
        }
        message
    }
}
impl ResponsesMessageView for Message {
    type Content = Content;
    type ToolCall = ToolCall;
    fn role(&self) -> ResponsesRole {
        self.role
    }
    fn contents(&self) -> &[Content] {
        &self.content
    }
    fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
    fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    fn content(&self) -> &str {
        &self.content[0].0
    }
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
    fn provider_content(&self) -> &str {
        &self.provider_content
    }
}
struct Request {
    messages: Vec<Message>,
    model: String,
    instructions: String,
    max_tokens: i32,
}
impl ResponsesRequestView for Request {
    type Message = Message;
    type Tool = Empty;
    fn model(&self) -> &str {
        &self.model
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[Empty] {
        &[]
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn max_output_tokens(&self) -> i32 {
        self.max_tokens
    }
    fn reasoning_effort(&self) -> &str {
        ""
    }
    fn reasoning_summary(&self) -> &str {
        ""
    }
    fn provider_preferences(&self) -> &str {
        ""
    }
    fn include_router_metadata(&self) -> bool {
        false
    }
}
pub fn request(
    action: &Value,
    previous: Option<&ResponsesCompletion>,
    provider_id: &str,
    chatgpt: bool,
) -> WebSocketRequest {
    let mut messages: Vec<_> = if let Some(count) = action["message_text_bytes"].as_u64() {
        vec![Message::text(
            ResponsesRole::User,
            "x".repeat(count as usize),
        )]
    } else {
        action["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(Message::from_input)
            .collect()
    };
    if action["append_previous_completion"]
        .as_bool()
        .unwrap_or(false)
    {
        messages.push(Message::from_completion(
            previous.expect("no actual previous completion to replay"),
        ));
    }
    if let Some(tail) = action["tail_messages"].as_array() {
        messages.extend(tail.iter().map(Message::from_input));
    }
    WebSocketRequest::new(
        &prepare(
            &Request {
                messages,
                model: action["model"].as_str().unwrap_or("model").into(),
                instructions: action["instructions"].as_str().unwrap_or_default().into(),
                // This is the source ChatGpt OpenSession caller boundary, not T30 routing implementation.
                max_tokens: if chatgpt {
                    0
                } else {
                    action["max_tokens"].as_i64().unwrap_or_default() as i32
                },
            },
            provider_id,
        )
        .unwrap(),
    )
    .unwrap()
}

struct SocketRecord {
    id: String,
    connected: bool,
    state: SocketState,
    aborted: bool,
    abort_calls: usize,
    dispose_calls: usize,
    sent_frames: usize,
    receive_calls: usize,
    messages: VecDeque<Vec<u8>>,
    offset: usize,
    sending: Vec<u8>,
    requests: Vec<Value>,
}
impl SocketRecord {
    fn snapshot(&self, requests: bool) -> Value {
        let mut result = json!({"socket_id": self.id, "connected": self.connected,
            "state": format!("{:?}", self.state), "aborted": self.aborted,
            "abort_calls": self.abort_calls, "dispose_calls": self.dispose_calls,
            "sent_messages": self.requests.len(), "sent_frames": self.sent_frames,
            "remaining_response_messages": self.messages.len(), "receive_calls": self.receive_calls});
        if requests {
            result["requests"] = self.requests.clone().into();
        }
        result
    }
}
struct Socket {
    record: Arc<Mutex<SocketRecord>>,
}
impl SocketTrait for Socket {
    fn state(&self) -> SocketState {
        self.record.lock().unwrap().state
    }
    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        end: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            let mut record = self.record.lock().unwrap();
            record.sending.extend_from_slice(data);
            record.sent_frames += 1;
            if end {
                let bytes = std::mem::take(&mut record.sending);
                record.requests.push(body(&bytes));
            }
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        self.record.lock().unwrap().receive_calls += 1;
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            let mut record = self.record.lock().unwrap();
            let message = record
                .messages
                .front()
                .expect("script ran out of socket messages");
            let count = buffer.len().min(message.len() - record.offset);
            buffer[..count].copy_from_slice(&message[record.offset..record.offset + count]);
            let end = record.offset + count == message.len();
            if end {
                record.messages.pop_front();
                record.offset = 0;
            } else {
                record.offset += count;
            }
            Ok(ReceivedFrame {
                count,
                kind: FrameKind::Text,
                end_of_message: end,
                close_status: None,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {
        let mut record = self.record.lock().unwrap();
        record.abort_calls += 1;
        record.aborted = true;
        record.state = SocketState::Aborted;
    }
    fn dispose(&mut self) {
        let mut record = self.record.lock().unwrap();
        record.dispose_calls += 1;
        record.state = SocketState::Closed;
    }
}
type ConnectOutcome = Result<(Arc<Mutex<SocketRecord>>, Headers), u16>;
struct Connector {
    outcomes: Mutex<VecDeque<ConnectOutcome>>,
    sockets: Vec<Arc<Mutex<SocketRecord>>>,
    calls: Mutex<Vec<Value>>,
}
impl Connector {
    fn new(configured: &Value) -> Self {
        let mut outcomes = VecDeque::new();
        let mut sockets = Vec::new();
        for outcome in configured.as_array().unwrap() {
            if let Some(status) = outcome["upgrade_status"].as_u64() {
                outcomes.push_back(Err(status as u16));
            } else {
                let socket = Arc::new(Mutex::new(SocketRecord {
                    id: outcome["socket_id"].as_str().unwrap().into(),
                    connected: false,
                    state: SocketState::Open,
                    aborted: false,
                    abort_calls: 0,
                    dispose_calls: 0,
                    sent_frames: 0,
                    receive_calls: 0,
                    messages: outcome["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|message| message.as_str().unwrap().as_bytes().to_vec())
                        .collect(),
                    offset: 0,
                    sending: vec![],
                    requests: vec![],
                }));
                sockets.push(socket.clone());
                let headers = outcome["headers"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap().into()))
                    .collect();
                outcomes.push_back(Ok((socket, headers)));
            }
        }
        Self {
            outcomes: Mutex::new(outcomes),
            sockets,
            calls: Mutex::new(vec![]),
        }
    }
    fn snapshot(&self, requests: bool) -> Value {
        self.sockets
            .iter()
            .map(|socket| socket.lock().unwrap().snapshot(requests))
            .collect::<Vec<_>>()
            .into()
    }
}
impl ConnectorTrait for Connector {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
        headers: &'a Headers,
        timeout: Option<Duration>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        let mut call = json!({"endpoint":endpoint, "headers": headers.iter().filter(|(key, _)| !key.eq_ignore_ascii_case("session-id")).map(|(key, value)| (key.clone(), Value::String(value.clone()))).collect::<serde_json::Map<_, _>>(),
            "timeout_ticks": timeout.unwrap().as_nanos() / 100, "token_cancelled": cancellation.is_cancelled()});
        if let Some((_, session_id)) = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("session-id"))
        {
            call["session_id_present"] = true.into();
            call["session_id_length"] = session_id.len().into();
        }
        self.calls.lock().unwrap().push(call);
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected connector call");
        Box::pin(async move {
            let (socket, response_headers) = outcome.map_err(|status| WebSocketError::Upgrade {
                status,
                message: "scripted unsupported upgrade".into(),
            })?;
            socket.lock().unwrap().connected = true;
            Ok(ConnectedSocket {
                socket: Box::new(Socket { record: socket }),
                response_headers,
            })
        })
    }
}
struct Auth {
    calls: AtomicUsize,
    chatgpt: bool,
}
impl HeaderSource for Auth {
    fn headers<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Headers, WebSocketError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            // Synthetic source-equivalent caller setup, not OAuth/routing implementation.
            if self.chatgpt {
                Ok(vec![
                    (
                        "Authorization".into(),
                        "Bearer synthetic-access-token".into(),
                    ),
                    ("originator".into(), "parrot".into()),
                    ("User-Agent".into(), "parrot".into()),
                    ("ChatGPT-Account-Id".into(), "synthetic-account-id".into()),
                    (
                        "session-id".into(),
                        "0123456789abcdef0123456789abcdef".into(),
                    ),
                ])
            } else {
                Ok(vec![
                    (
                        "Authorization".into(),
                        "Bearer synthetic-session-key".into(),
                    ),
                    ("User-Agent".into(), "parrot/0.0.0-dev".into()),
                ])
            }
        })
    }
}
struct Http {
    requests: Mutex<Vec<Value>>,
    states: Vec<String>,
    auth: Arc<Auth>,
    provider_id: String,
    response_body: String,
}
struct HttpBytes {
    bytes: Vec<u8>,
    offset: usize,
}
impl ByteStream for HttpBytes {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct HttpEvents {
    events: VecDeque<ResponsesEvent>,
    state: Option<String>,
}
impl HttpEventsTrait for HttpEvents {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(WebSocketError::Cancelled)
            } else {
                Ok(self.events.pop_front())
            }
        })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        self.state.take()
    }
}
impl HttpFallback for Http {
    fn open<'a>(
        &'a self,
        bytes: Vec<u8>,
        turn_state: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEventsTrait>, WebSocketError>> {
        self.auth.calls.fetch_add(1, Ordering::SeqCst);
        let mut requests = self.requests.lock().unwrap();
        let state = self.states.get(requests.len()).cloned();
        requests.push(json!({"body":body(&bytes), "turn_state":turn_state}));
        Box::pin(async move {
            let mut bytes = HttpBytes {
                bytes: self.response_body.as_bytes().to_vec(),
                offset: 0,
            };
            let mut stream = ResponsesEventStream::new(&mut bytes, 4 << 20, &self.provider_id);
            let mut events = VecDeque::new();
            loop {
                match stream.next(cancellation).await {
                    Ok(Some(event)) => events.push_back(event),
                    Ok(None) => break,
                    Err(ResponsesStreamError::Protocol(error)) => return Err(error.into()),
                    Err(ResponsesStreamError::Transport(error)) => {
                        return Err(WebSocketError::Transport(error.to_string()));
                    }
                }
            }
            Ok(Box::new(HttpEvents { events, state }) as Box<dyn HttpEventsTrait>)
        })
    }
}

fn event(event: SessionEvent) -> Value {
    let mut row = json!({"Kind":"", "Text":"", "ReasoningKind":"Raw", "ReasoningPartId":"", "ReasoningCompleted":false,
        "ToolCallId":"", "ToolName":"", "Attempt":0, "RetryAfter":"00:00:00", "FinishReason":"",
        "InputTokens":0, "CachedInputTokens":0, "OutputTokens":0, "ToolCalls":[], "AssistantText":"",
        "ProviderContent":{"ProviderId":"", "Content":""}, "IsVisibleOutput":false});
    match event {
        SessionEvent::HttpRequestStarted => row["Kind"] = "HttpRequestStarted".into(),
        SessionEvent::HttpResponseHeadersReceived => {
            row["Kind"] = "HttpResponseHeadersReceived".into();
        }
        SessionEvent::Retry { attempt, reason } => {
            row["Kind"] = "Retry".into();
            row["Attempt"] = attempt.into();
            row["Text"] = reason.into();
        }
        SessionEvent::Response(response) => {
            row["IsVisibleOutput"] =
                (!matches!(response, ResponsesEvent::EncryptedReasoningReceived)).into();
            match response {
                ResponsesEvent::TextDelta(text) => {
                    row["Kind"] = "TextDelta".into();
                    row["Text"] = text.into();
                }
                ResponsesEvent::ReasoningDelta {
                    text,
                    kind,
                    part_id,
                    completed,
                } => {
                    row["Kind"] = "ReasoningDelta".into();
                    row["Text"] = text.into();
                    row["ReasoningKind"] = format!("{kind:?}").into();
                    row["ReasoningPartId"] = part_id.into();
                    row["ReasoningCompleted"] = completed.into();
                }
                ResponsesEvent::EncryptedReasoningReceived => {
                    row["Kind"] = "EncryptedReasoningReceived".into()
                }
                ResponsesEvent::ToolCallDelta(call) => {
                    row["Kind"] = "ToolCallDelta".into();
                    row["ToolCallId"] = call.id.into();
                    row["ToolName"] = call.name.into();
                    row["Text"] = call.arguments_json.into();
                }
                ResponsesEvent::Completed(completion) => {
                    row["Kind"] = "Completed".into();
                    row["FinishReason"] = completion.finish_reason.into();
                    row["InputTokens"] = completion.input_tokens.into();
                    row["CachedInputTokens"] = completion.cached_input_tokens.into();
                    row["OutputTokens"] = completion.output_tokens.into();
                    row["AssistantText"] = completion.assistant_text.into();
                    row["ToolCalls"] = completion.tool_calls.into_iter().map(|call| json!({"Id":call.id,"Name":call.name,"ArgumentsJson":call.arguments_json})).collect::<Vec<_>>().into();
                    if let Some(content) = completion.provider_content {
                        row["ProviderContent"] =
                            json!({"ProviderId":completion.provider_id,"Content":content});
                    }
                }
            }
        }
    }
    row
}
fn failure(error: WebSocketError, cancellation: &Cancel) -> Value {
    match error {
        WebSocketError::Cancelled => {
            json!({"kind":"cancelled", "cancellation_requested":cancellation.is_cancelled()})
        }
        WebSocketError::Response(ResponsesError::ProviderResponse {
            error_type,
            error_code,
            detail,
            response_body,
        }) => {
            let error = ResponsesError::ProviderResponse {
                error_type: error_type.clone(),
                error_code: error_code.clone(),
                detail: detail.clone(),
                response_body: response_body.clone(),
            };
            json!({"kind":"provider_response", "message":error.to_string(), "error_type":error_type, "error_code":error_code, "detail":detail, "response_body":response_body})
        }
        error => panic!("unexpected observer error: {error:?}"),
    }
}
fn snapshot(connector: &Connector, http: &Http, auth: &Auth, tick: u64) -> Value {
    json!({"tick":tick, "connector_calls":connector.calls.lock().unwrap().len(), "http_calls":http.requests.lock().unwrap().len(),
        "auth_calls":auth.calls.load(Ordering::SeqCst), "sockets":connector.snapshot(false)})
}
// This is deliberately input-only; oracle data never enters the execution path.
pub async fn observe(input: &Value) -> Value {
    observe_completions(input, |_| {}).await
}
pub async fn observe_completions(
    input: &Value,
    mut capture: impl FnMut(&ResponsesCompletion),
) -> Value {
    let connector = Arc::new(Connector::new(&input["outcomes"]));
    let chatgpt = input["provider_kind"].as_str() == Some("chatgpt");
    let provider_id = if chatgpt {
        "chatgpt"
    } else {
        input["provider_id"].as_str().unwrap_or("oracle-provider")
    };
    let auth = Arc::new(Auth {
        calls: AtomicUsize::new(0),
        chatgpt,
    });
    let http = Http {
        requests: Mutex::new(vec![]),
        states: input["http_turn_states"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap().into())
                    .collect()
            })
            .unwrap_or_default(),
        auth: auth.clone(),
        provider_id: provider_id.into(),
        response_body: input["http_response_body"].as_str().unwrap_or("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"http-response\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"output\":[]}}\n\n").into(),
    };
    let client = WebSocketClient::new(
        connector.clone(),
        if chatgpt {
            "https://chatgpt.com/backend-api/codex/responses"
        } else {
            "https://session.example.test/v1/responses"
        },
        Some(Duration::from_millis(
            input["header_timeout_ms"].as_u64().unwrap_or(60000),
        )),
        None,
        i32::MAX as usize,
    )
    .unwrap();
    let mut session = Session::new(
        provider_id,
        auth.clone(),
        client,
        if chatgpt {
            false
        } else {
            input["disable_websocket"].as_bool().unwrap_or(true)
        },
    );
    let mut previous_completion = None;
    let mut tick = 0;
    let initial = snapshot(&connector, &http, &auth, tick);
    let mut actions = vec![];
    for action in input["actions"].as_array().unwrap() {
        let mut row = json!({"action":action["action"]});
        match action["action"].as_str().unwrap() {
            "begin_turn" => session.begin_turn(),
            "advance_seconds" => {
                let seconds = action["seconds"].as_u64().unwrap();
                tick += seconds * 10_000_000;
                tokio::time::advance(Duration::from_secs(seconds)).await;
            }
            "call" => {
                let cancellation = Cancel(AtomicBool::new(false));
                let mut events = vec![];
                let mut exception = Value::Null;
                {
                    let mut call = session
                        .call(
                            request(action, previous_completion.as_ref(), provider_id, chatgpt),
                            &http,
                        )
                        .unwrap();
                    loop {
                        match call.next(&cancellation).await {
                            Ok(Some(published)) => {
                                if let SessionEvent::Response(ResponsesEvent::Completed(
                                    completion,
                                )) = &published
                                {
                                    capture(completion);
                                    previous_completion = Some(completion.clone());
                                }
                                events.push(event(published));
                                if action["cancel_after_events"].as_u64()
                                    == Some(events.len() as u64)
                                {
                                    cancellation.0.store(true, Ordering::SeqCst);
                                }
                                if action["stop_after_events"].as_u64() == Some(events.len() as u64)
                                {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                exception = failure(error, &cancellation);
                                break;
                            }
                        }
                    }
                }
                row["events"] = events.into();
                row["exception"] = exception;
            }
            action => panic!("unknown input action {action}"),
        }
        row["state"] = snapshot(&connector, &http, &auth, tick);
        actions.push(row);
    }
    let sockets = connector.snapshot(true);
    session.dispose();
    let calls = connector.calls.lock().unwrap().clone();
    let requests = http.requests.lock().unwrap().clone();
    json!({"initial_state":initial, "actions":actions, "connector_calls":calls,
        "http_requests":requests, "sockets_before_session_dispose":sockets,
        "after_session_dispose":snapshot(&connector, &http, &auth, tick)})
}
#[cfg(test)]
fn reference_state(state: &Value) -> Value {
    let mut state = state.clone();
    for key in [
        "http_only",
        "turn_state",
        "reset_connection",
        "lineage_response_id",
    ] {
        state.as_object_mut().unwrap().remove(key);
    }
    state
}
#[cfg(test)]
fn project_reference(reference: &Value) -> Value {
    let actions = reference["actions"].as_array().unwrap().iter().map(|action| {
        let mut row = json!({"action":action["action"], "state":reference_state(&action["state"])});
        if action["action"] == "call" {
            row["events"] = action["events"].as_array().unwrap().iter().filter(|event| !matches!(event["Kind"].as_str(), Some("HttpRequestStarted" | "HttpResponseHeadersReceived"))).cloned().collect::<Vec<_>>().into();
            row["exception"] = if action["exception"].is_null() { Value::Null } else {
                let exception = &action["exception"];
                match exception["type"].as_str().unwrap() {
                    "System.OperationCanceledException" => json!({"kind":"cancelled", "cancellation_requested":exception["cancellation_requested"]}),
                    "Parrot.Llm.Wire.ProviderResponseException" => json!({"kind":"provider_response", "message":exception["message"], "error_type":exception["error_type"], "error_code":exception["error_code"], "detail":exception["detail"], "response_body":exception["response_body"]}),
                    kind => panic!("unmapped reference exception {kind}"),
                }
            };
        }
        row
    }).collect::<Vec<_>>();
    let http = reference["http_requests"].as_array().unwrap().iter().map(|request| json!({"body":request["body"], "turn_state":request["headers"]["x-codex-turn-state"].as_str().unwrap_or_default()})).collect::<Vec<_>>();
    json!({"initial_state":reference_state(&reference["initial_state"]), "actions":actions,
        "connector_calls":reference["connector_calls"], "http_requests":http,
        "sockets_before_session_dispose":reference["sockets_before_session_dispose"],
        "after_session_dispose":reference_state(&reference["after_session_dispose"])})
}

#[tokio::test(start_paused = true)]
async fn source_session_oracle_matches_all_applicable_observable_fields() {
    assert_source_cases(INPUTS, ORACLE, (17, 19, 50, 85)).await;
}

#[tokio::test(start_paused = true)]
async fn source_session_v2_native_history_matches_all_applicable_observable_fields() {
    let inputs = include_str!("fixtures/session/inputs.json");
    let oracle = include_str!("fixtures/session/expected.json");
    assert_source_cases(inputs, oracle, (30, 32, 73, 137)).await;
}

#[cfg(test)]
async fn assert_source_cases(inputs: &str, oracle: &str, counts: (usize, usize, usize, usize)) {
    let inputs: Value = serde_json::from_str(inputs).unwrap();
    let oracle: Value = serde_json::from_str(oracle).unwrap();
    assert_eq!(inputs["source_commit"], oracle["source_commit"]);
    let inputs = inputs["scenarios"].as_array().unwrap();
    let references = oracle["scenarios"].as_array().unwrap();
    assert_eq!(inputs.len(), references.len());
    let mut sessions = 0;
    let mut calls = 0;
    for (input, reference) in inputs.iter().zip(references) {
        assert_eq!(input["case_id"], reference["case_id"]);
        if let Some(variants) = input["variants"].as_array() {
            let expected = reference["variants"].as_array().unwrap();
            assert_eq!(variants.len(), expected.len());
            for (variant, expected) in variants.iter().zip(expected) {
                assert_eq!(variant["variant_id"], expected["variant_id"]);
                assert_eq!(
                    observe(variant).await,
                    project_reference(expected),
                    "{} / {}",
                    input["case_id"],
                    variant["variant_id"]
                );
                sessions += 1;
                calls += variant["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|action| action["action"] == "call")
                    .count();
            }
        } else {
            assert_eq!(
                observe(input).await,
                project_reference(&reference["session"]),
                "{}",
                input["case_id"]
            );
            sessions += 1;
            calls += input["actions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|action| action["action"] == "call")
                .count();
        }
    }
    let source_events = references
        .iter()
        .flat_map(|scenario| {
            scenario["variants"]
                .as_array()
                .map(|variants| variants.iter().collect::<Vec<_>>())
                .unwrap_or_else(|| vec![&scenario["session"]])
        })
        .flat_map(|session| session["actions"].as_array().unwrap())
        .filter_map(|action| action["events"].as_array())
        .map(Vec::len)
        .sum::<usize>();
    assert_eq!((inputs.len(), sessions, calls, source_events), counts);
}
