use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::{ByteStream, HttpError};
use parrot_provider::responses::{
    ResponsesContentKind, ResponsesContentView, ResponsesError, ResponsesEvent,
    ResponsesEventStream, ResponsesMessageView, ResponsesRequestView, ResponsesRole,
    ResponsesStreamError, ResponsesToolCallView, ResponsesToolDefinitionView, prepare,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Request {
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    max_output_tokens: i32,
    #[serde(default)]
    provider_preferences: String,
    #[serde(default)]
    include_router_metadata: bool,
    #[serde(default)]
    reasoning: Option<Reasoning>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    tools: Vec<ToolDefinition>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Reasoning {
    #[serde(default)]
    effort: String,
    #[serde(default)]
    summary: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    #[serde(default)]
    role: String,
    #[serde(default)]
    contents: Vec<Content>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    tool_call_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    provider_content: Option<ProviderContent>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderContent {
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    content: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Content {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    path: PathBuf,
    #[serde(default)]
    bytes_base64: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments_json: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters_json: String,
}

impl ResponsesRequestView for Request {
    type Message = Message;
    type Tool = ToolDefinition;
    fn model(&self) -> &str {
        &self.model
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
    fn max_output_tokens(&self) -> i32 {
        self.max_output_tokens
    }
    fn reasoning_effort(&self) -> &str {
        self.reasoning.as_ref().map_or("", |value| &value.effort)
    }
    fn reasoning_summary(&self) -> &str {
        self.reasoning.as_ref().map_or("", |value| &value.summary)
    }
    fn provider_preferences(&self) -> &str {
        &self.provider_preferences
    }
    fn include_router_metadata(&self) -> bool {
        self.include_router_metadata
    }
}
impl ResponsesMessageView for Message {
    type Content = Content;
    type ToolCall = ToolCall;
    fn role(&self) -> ResponsesRole {
        match self.role.as_str() {
            "System" => ResponsesRole::System,
            "Assistant" => ResponsesRole::Assistant,
            "Tool" => ResponsesRole::Tool,
            _ => ResponsesRole::User,
        }
    }
    fn contents(&self) -> &[Content] {
        &self.contents
    }
    fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }
    fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    fn content(&self) -> &str {
        &self.content
    }
    fn provider_id(&self) -> &str {
        self.provider_content
            .as_ref()
            .map_or("", |value| &value.provider_id)
    }
    fn provider_content(&self) -> &str {
        self.provider_content
            .as_ref()
            .map_or("", |value| &value.content)
    }
}
impl ResponsesContentView for Content {
    fn kind(&self) -> ResponsesContentKind {
        if self.kind == "image_file" || self.kind == "image_link" {
            ResponsesContentKind::Image
        } else {
            ResponsesContentKind::Text
        }
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn image_path(&self) -> &Path {
        &self.path
    }
    fn image_url(&self) -> &str {
        &self.url
    }
    fn image_media_type(&self) -> &str {
        &self.media_type
    }
    fn read_image(&self) -> Result<Vec<u8>, io::Error> {
        decode_base64(&self.bytes_base64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid synthetic base64"))
    }
}
impl ResponsesToolCallView for ToolCall {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn arguments_json(&self) -> &str {
        &self.arguments_json
    }
}
impl ResponsesToolDefinitionView for ToolDefinition {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_json(&self) -> &str {
        &self.parameters_json
    }
}

pub fn observe_outbound(input: &Value) -> Value {
    let mut fixture = input.clone();
    if let Some(messages) = fixture["messages"].as_array_mut() {
        for message in messages
            .iter_mut()
            .filter(|message| message["role"] == "Tool")
        {
            if message.get("content").is_none() {
                let content = message["contents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>();
                message["content"] = Value::String(content);
            }
        }
    }
    let request: Request =
        serde_json::from_value(fixture).expect("fixture request maps to borrowed test view");
    let provider_id = input["provider_id"].as_str().unwrap_or_default();
    let prepared = prepare(&request, provider_id).expect("source-valid request fixture");
    let body = prepared.body();
    let parsed: Value = serde_json::from_str(body).expect("prepared JSON is valid");
    json!({
        "prepared_provider_id":provider_id,
        "prepared_input":parsed["input"],
        "prepared_body":parsed,
        "prepared_http_utf8":body,
        "prepared_http_base64":encode_base64(body.as_bytes()),
        "encoded_utf8":body,
        "encoded_base64":encode_base64(body.as_bytes()),
    })
}

pub fn observe_stream_async(input: &Value) -> (Vec<Value>, Option<Value>) {
    observe_stream_async_for_provider(input, "oracle-provider")
}

pub fn observe_stream_async_for_provider(
    input: &Value,
    provider_id: &str,
) -> (Vec<Value>, Option<Value>) {
    let sse = input["sse"].as_str().unwrap();
    let chunks = sse.as_bytes().to_vec();
    let cancellation = TestCancellation::new(input["cancel_before"].as_bool().unwrap_or(false));
    let mut source = MemoryStream {
        bytes: chunks,
        position: 0,
    };
    let mut stream = ResponsesEventStream::new(&mut source, 1_048_576, provider_id);
    let cancel_after = input["cancel_after_events"].as_i64().unwrap_or(-1);
    let mut events = Vec::new();
    let mut exception = None;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    loop {
        let next = runtime.block_on(stream.next(&cancellation));
        match next {
            Ok(Some(event)) => {
                events.push(event_json(&event));
                if events.len() as i64 == cancel_after {
                    cancellation.cancel();
                }
            }
            Ok(None) => break,
            Err(error) => {
                exception = Some(stream_error_json(error));
                break;
            }
        }
    }
    (events, exception)
}

fn event_json(event: &ResponsesEvent) -> Value {
    let (
        kind,
        text,
        reasoning_kind,
        reasoning_id,
        reasoning_completed,
        tool_id,
        tool_name,
        finish,
        input,
        cached,
        output,
        assistant,
        provider,
        visible,
        calls,
    ) = match event {
        ResponsesEvent::TextDelta(text) => (
            "TextDelta",
            text.as_str(),
            "Raw",
            "",
            false,
            "",
            "",
            "",
            0,
            0,
            0,
            "",
            json!({"ProviderId":"","Content":""}),
            true,
            vec![],
        ),
        ResponsesEvent::ReasoningDelta {
            text,
            kind,
            part_id,
            completed,
        } => (
            "ReasoningDelta",
            text.as_str(),
            if *kind == parrot_provider::responses::ResponsesReasoningKind::Summary {
                "Summary"
            } else {
                "Raw"
            },
            part_id.as_str(),
            *completed,
            "",
            "",
            "",
            0,
            0,
            0,
            "",
            json!({"ProviderId":"","Content":""}),
            true,
            vec![],
        ),
        ResponsesEvent::EncryptedReasoningReceived => (
            "EncryptedReasoningReceived",
            "",
            "Raw",
            "",
            false,
            "",
            "",
            "",
            0,
            0,
            0,
            "",
            json!({"ProviderId":"","Content":""}),
            false,
            vec![],
        ),
        ResponsesEvent::ToolCallDelta(call) => (
            "ToolCallDelta",
            call.arguments_json.as_str(),
            "Raw",
            "",
            false,
            call.id.as_str(),
            call.name.as_str(),
            "",
            0,
            0,
            0,
            "",
            json!({"ProviderId":"","Content":""}),
            true,
            vec![],
        ),
        ResponsesEvent::Completed(completion) => {
            let calls = completion.tool_calls.iter().map(|call| json!({"Id":call.id,"Name":call.name,"ArgumentsJson":call.arguments_json})).collect::<Vec<_>>();
            (
                "Completed",
                "",
                "Raw",
                "",
                false,
                "",
                "",
                completion.finish_reason.as_str(),
                completion.input_tokens,
                completion.cached_input_tokens,
                completion.output_tokens,
                completion.assistant_text.as_str(),
                completion.provider_content.as_ref().map_or_else(
                    || json!({"ProviderId":"","Content":""}),
                    |content| json!({"ProviderId":completion.provider_id,"Content":content}),
                ),
                true,
                calls,
            )
        }
    };
    json!({"Kind":kind,"Text":text,"ReasoningKind":reasoning_kind,"ReasoningPartId":reasoning_id,"ReasoningCompleted":reasoning_completed,"ToolCallId":tool_id,"ToolName":tool_name,"Attempt":0,"RetryAfter":"00:00:00","FinishReason":finish,"InputTokens":input,"CachedInputTokens":cached,"OutputTokens":output,"ToolCalls":calls,"AssistantText":assistant,"ProviderContent":provider,"IsVisibleOutput":visible})
}
fn json_error_location(message: &str) -> Option<(u64, u64)> {
    let (_, location) = message.rsplit_once(" at line ")?;
    let (line, column) = location.split_once(" column ")?;
    let line_number = line.parse::<u64>().ok()?.saturating_sub(1);
    let column = column.parse::<u64>().ok()?;
    let byte_position = if message.starts_with("EOF ") {
        column
    } else {
        column.saturating_sub(1)
    };
    Some((line_number, byte_position))
}

fn error_json(error: ResponsesError) -> Value {
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
            let message = if detail.is_empty() {
                "provider error".to_owned()
            } else {
                detail.clone()
            };
            let message = if error_type.is_empty() {
                message
            } else {
                format!("{message} ({error_type})")
            };
            let message = if error_code.is_empty() {
                message
            } else {
                format!("{message} [{error_code}]")
            };
            json!({"type":"Parrot.Llm.Wire.ProviderResponseException","message":message,"error_type":error_type,"error_code":error_code,"detail":detail,"response_body":response_body})
        }
        ResponsesError::Json(message) => {
            let (line_number, byte_position_in_line) = json_error_location(&message)
                .expect("serde_json diagnostics include line and column");
            json!({"type":"System.Text.Json.JsonReaderException","line_number":line_number,"byte_position_in_line":byte_position_in_line})
        }
        ResponsesError::InvalidInteger { field: _, .. } => {
            json!({"type":"Parrot.Llm.Wire.WireProtocolException","message":"responses: sequence_number must be an integer"})
        }
    }
}
fn stream_error_json(error: ResponsesStreamError) -> Value {
    match error {
        ResponsesStreamError::Protocol(error) => error_json(error),
        ResponsesStreamError::Transport(HttpError::Cancelled) => {
            json!({"type":"System.Threading.Tasks.TaskCanceledException","message":"A task was canceled."})
        }
        ResponsesStreamError::Transport(HttpError::Wire(message)) => {
            json!({"type":"Parrot.Llm.Wire.WireProtocolException","message":message})
        }
        ResponsesStreamError::Transport(error) => {
            json!({"type":"TransportError","message":error.to_string()})
        }
    }
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    let mut block = 0_u32;
    let mut bits = 0;
    let mut padding = false;
    for byte in input.bytes() {
        if byte == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        block = (block << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            result.push((block >> bits) as u8);
        }
    }
    Some(result)
}
fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

pub struct TestCancellation(Arc<AtomicBool>);
impl TestCancellation {
    fn new(cancelled: bool) -> Self {
        Self(Arc::new(AtomicBool::new(cancelled)))
    }
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl Cancellation for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        let token = self.0.clone();
        Box::pin(async move {
            while !token.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
    }
}
struct MemoryStream {
    bytes: Vec<u8>,
    position: usize,
}
impl ByteStream for MemoryStream {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let length = buffer.len().min(self.bytes.len() - self.position);
            buffer[..length].copy_from_slice(&self.bytes[self.position..self.position + length]);
            self.position += length;
            Ok(length)
        })
    }
}
