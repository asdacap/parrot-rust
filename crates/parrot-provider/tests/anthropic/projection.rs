//! Input-only observations through production APIs. CLR labels below identify
//! the independent source contract, not Rust runtime exception types.
#[path = "../../src/anthropic/wire_json.rs"]
mod wire_json;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{anthropic::*, catalogs::Model, http_stream::*};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}
fn n(value: &Value, key: &str) -> i64 {
    value[key].as_i64().unwrap()
}
fn b(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap()
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}
fn unhex(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap())
        .collect()
}

struct Json<'a>(&'a Value);
impl std::ops::Deref for Json<'_> {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.0
    }
}
struct Request<'a> {
    value: &'a Value,
    messages: Vec<Message<'a>>,
    tools: Vec<Json<'a>>,
}
struct Message<'a> {
    value: &'a Value,
    contents: Vec<Json<'a>>,
    calls: Vec<Json<'a>>,
}
impl<'a> Request<'a> {
    fn new(value: &'a Value) -> Self {
        Self {
            value,
            messages: value["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| Message {
                    value,
                    contents: value["contents"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(Json)
                        .collect(),
                    calls: value["tool_calls"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(Json)
                        .collect(),
                })
                .collect(),
            tools: value["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(Json)
                .collect(),
        }
    }
}
impl<'a> RequestView for Request<'a> {
    type Message = Message<'a>;
    type Tool = Json<'a>;
    fn model(&self) -> &str {
        s(self.value, "model")
    }
    fn max_output_tokens(&self) -> i32 {
        n(self.value, "max_output_tokens") as i32
    }
    fn instructions(&self) -> &str {
        s(self.value, "instructions")
    }
    fn messages(&self) -> &[Self::Message] {
        &self.messages
    }
    fn tools(&self) -> &[Self::Tool] {
        &self.tools
    }
    fn reasoning_effort(&self) -> Option<&str> {
        self.value["reasoning"]["effort"].as_str()
    }
    fn reasoning_summary(&self) -> Option<&str> {
        self.value["reasoning"]["summary"].as_str()
    }
    fn provider_preferences(&self) -> &str {
        s(self.value, "provider_preferences")
    }
    fn include_router_metadata(&self) -> bool {
        b(self.value, "include_router_metadata")
    }
}
impl<'a> MessageView for Message<'a> {
    type Content = Json<'a>;
    type ToolCall = Json<'a>;
    fn role(&self) -> Role {
        match s(self.value, "role") {
            "System" => Role::System,
            "User" => Role::User,
            "Assistant" => Role::Assistant,
            "Tool" => Role::Tool,
            other => panic!("unknown role {other}"),
        }
    }
    fn contents(&self) -> &[Self::Content] {
        &self.contents
    }
    fn tool_calls(&self) -> &[Self::ToolCall] {
        &self.calls
    }
    fn tool_call_id(&self) -> &str {
        s(self.value, "tool_call_id")
    }
    fn provider_id(&self) -> &str {
        s(&self.value["provider_content"], "provider_id")
    }
    fn provider_content(&self) -> &str {
        s(&self.value["provider_content"], "content")
    }
}
impl ContentView for Json<'_> {
    fn kind(&self) -> ContentKind {
        if s(self, "kind") == "text" {
            ContentKind::Text
        } else {
            ContentKind::Image
        }
    }
    fn text(&self) -> &str {
        s(self, "text")
    }
    fn image_path(&self) -> &str {
        "anthropic-image.bin"
    }
    fn image_url(&self) -> &str {
        s(self, "image_url")
    }
    fn media_type(&self) -> &str {
        s(self, "media_type")
    }
    fn image_width(&self) -> i32 {
        n(self, "width") as i32
    }
    fn image_height(&self) -> i32 {
        n(self, "height") as i32
    }
    fn read_image(&self) -> std::io::Result<Vec<u8>> {
        Ok(unhex(s(self, "image_hex")))
    }
}
impl ToolDefinitionView for Json<'_> {
    fn name(&self) -> &str {
        s(self, "name")
    }
    fn description(&self) -> &str {
        s(self, "description")
    }
    fn parameters_json(&self) -> &str {
        s(self, "parameters_json")
    }
}
impl ToolCallView for Json<'_> {
    fn id(&self) -> &str {
        s(self, "id")
    }
    fn name(&self) -> &str {
        s(self, "name")
    }
    fn arguments_json(&self) -> &str {
        s(self, "arguments_json")
    }
}

struct Token(tokio::sync::watch::Sender<bool>);
impl Token {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut rx = self.0.subscribe();
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}
struct Key {
    value: String,
    api: AtomicUsize,
    has: AtomicUsize,
}
impl ApiKeySource for Key {
    fn api_key<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        Box::pin(async move {
            self.api.fetch_add(1, Ordering::SeqCst);
            if c.is_cancelled() {
                Err(HttpError::Cancelled.into())
            } else {
                Ok(self.value.clone())
            }
        })
    }
    fn has_credential<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, ProviderError>> {
        Box::pin(async move {
            self.has.fetch_add(1, Ordering::SeqCst);
            if c.is_cancelled() {
                Err(HttpError::Cancelled.into())
            } else {
                Ok(!self.value.is_empty())
            }
        })
    }
}
struct Body {
    bytes: Vec<u8>,
    position: usize,
    reads: usize,
    chunks: Vec<usize>,
    disposed: Arc<AtomicBool>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if c.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let limit = if self.chunks.is_empty() {
                usize::MAX
            } else {
                let n = self.chunks[self.reads % self.chunks.len()];
                self.reads += 1;
                n
            };
            let count = buffer
                .len()
                .min(self.bytes.len() - self.position)
                .min(limit);
            buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        })
    }
}
struct Transport {
    body: Mutex<Option<Body>>,
    status: u16,
    content_type: String,
    requests: Mutex<Vec<Value>>,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let mut headers = serde_json::Map::new();
            let mut content_headers = serde_json::Map::new();
            for (name, value) in request.headers {
                if name.eq_ignore_ascii_case("Content-Type") {
                    content_headers.insert(name, Value::String(value));
                } else {
                    headers.insert(name, Value::String(value));
                }
            }
            let is_post = request.method == HttpMethod::Post;
            if is_post {
                content_headers.insert(
                    "Content-Length".into(),
                    json!(request.body.len().to_string()),
                );
            }
            self.requests.lock().unwrap().push(json!({"method":if is_post {"POST"} else {"GET"},"uri":request.endpoint.as_str(),"headers":headers,"content_headers":if is_post {json!(content_headers)} else {Value::Null},"body_json":if request.body.is_empty(){Value::Null}else{serde_json::from_slice::<Value>(&request.body).unwrap()},"body_hex":hex(&request.body)}));
            Ok(HttpResponse {
                status: self.status,
                reason: match self.status {
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    _ => "",
                }
                .into(),
                headers: vec![("Content-Type".into(), self.content_type.clone())],
                body: Box::new(self.body.lock().unwrap().take().unwrap()),
            })
        })
    }
}

fn event(kind: &str) -> Value {
    json!({"Kind":kind,"Text":"","ReasoningKind":"Raw","ReasoningPartId":"","ReasoningCompleted":false,"ToolCallId":"","ToolName":"","Attempt":0,"RetryAfter":"00:00:00","FinishReason":"","InputTokens":0,"CachedInputTokens":0,"OutputTokens":0,"ToolCalls":[],"AssistantText":"","ProviderContent":{"ProviderId":"","Content":""},"IsVisibleOutput":matches!(kind,"TextDelta"|"ReasoningDelta"|"ToolCallDelta"|"Completed")})
}
fn tool(call: ToolCall) -> Value {
    json!({"Id":call.id,"Name":call.name,"ArgumentsJson":call.arguments_json})
}
fn message_event(value: AnthropicEvent) -> Value {
    match value {
        AnthropicEvent::TextDelta { text } => {
            let mut e = event("TextDelta");
            e["Text"] = json!(text);
            e
        }
        AnthropicEvent::ReasoningDelta {
            text,
            part_id,
            completed,
        } => {
            let mut e = event("ReasoningDelta");
            e["Text"] = json!(text);
            e["ReasoningKind"] = json!("Summary");
            e["ReasoningPartId"] = json!(part_id);
            e["ReasoningCompleted"] = json!(completed);
            e
        }
        AnthropicEvent::ToolCallDelta(call) => {
            let mut e = event("ToolCallDelta");
            e["Text"] = json!(call.arguments_json);
            e["ToolCallId"] = json!(call.id);
            e["ToolName"] = json!(call.name);
            e
        }
        AnthropicEvent::Completed(value) => {
            let mut e = event("Completed");
            e["FinishReason"] = json!(value.finish_reason);
            e["InputTokens"] = json!(value.input_tokens);
            e["CachedInputTokens"] = json!(value.cached_input_tokens);
            e["OutputTokens"] = json!(value.output_tokens);
            e["ToolCalls"] = json!(value.tool_calls.into_iter().map(tool).collect::<Vec<_>>());
            e["AssistantText"] = json!(value.assistant_text);
            e["ProviderContent"] =
                json!({"ProviderId":value.provider_id,"Content":value.provider_content});
            e
        }
    }
}
fn price(value: f64) -> Value {
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}
fn model(value: Model) -> Value {
    let labels = [
        "Name",
        "ContextWindow",
        "MaxOutputTokens",
        "InputPrice",
        "CachedInputPrice",
        "OutputPrice",
        "Tools",
        "Reasoning",
        "Output",
        "Variants",
        "MaxInputTokens",
    ];
    let fields = labels
        .iter()
        .enumerate()
        .filter(|(i, _)| value.fields.bits() & (1 << i) != 0)
        .map(|(_, v)| *v)
        .collect::<Vec<_>>();
    json!({"Id":value.id,"ProviderId":value.provider_id,"Name":value.name,"ContextWindow":value.context_window,"MaxOutputTokens":value.max_output_tokens,"MaxInputTokens":value.max_input_tokens,"InputTokenLimit":value.input_token_limit(),"InputPrice":price(value.input_price),"CachedInputPrice":price(value.cached_input_price),"OutputPrice":price(value.output_price),"Capabilities":{"Tools":value.capabilities.tools,"Reasoning":value.capabilities.reasoning,"Output":value.capabilities.output,"Variants":value.capabilities.variants.into_iter().map(|v|json!({"Name":v.name,"ReasoningEffort":v.reasoning_effort})).collect::<Vec<_>>()},"Fields":if fields.is_empty(){"None".into()}else{fields.join(", ")}})
}
fn error(value: ProviderError, operation: &str) -> Value {
    let mut result = json!({"category":"","message":value.to_string(),"status":null,"type":null,"code":null,"detail":null,"response_body":null});
    let category = match &value {
        ProviderError::Http(HttpError::Response(v)) => {
            result["status"] = json!(v.status);
            result["type"] = json!(v.error_type);
            result["code"] = json!(v.error_code);
            result["detail"] = json!(v.detail);
            result["response_body"] = json!(v.response_body);
            "Parrot.Llm.Wire.ProviderHttpException"
        }
        ProviderError::Parse(ParserError::ProviderResponse {
            error_type,
            error_code,
            detail,
            response_body,
        }) => {
            result["type"] = json!(error_type);
            result["code"] = json!(error_code);
            result["detail"] = json!(detail);
            result["response_body"] = json!(response_body);
            "Parrot.Llm.Wire.ProviderResponseException"
        }
        ProviderError::Http(HttpError::Cancelled) => {
            if operation == "parse" {
                result["message"] = json!("A task was canceled.");
                "System.Threading.Tasks.TaskCanceledException"
            } else {
                result["message"] = json!("The operation was canceled.");
                "System.OperationCanceledException"
            }
        }
        ProviderError::Http(HttpError::Wire(_)) | ProviderError::Parse(ParserError::Wire(_)) => {
            "Parrot.Llm.Wire.WireProtocolException"
        }
        ProviderError::Http(HttpError::RequestTooLarge(_)) => {
            "Parrot.Llm.Wire.ProviderRequestTooLargeException"
        }
        ProviderError::Http(HttpError::HeaderTimeout(_)) => {
            "Parrot.Llm.Wire.HeaderTimeoutException"
        }
        ProviderError::Parse(ParserError::Serialization(_))
        | ProviderError::Encode(EncodeError::Serialization(_)) => "System.Text.Json.JsonException",
        ProviderError::Parse(ParserError::Json(_)) => "System.Text.Json.JsonReaderException",
        ProviderError::Parse(ParserError::InvalidJsonString(_)) => {
            "System.InvalidOperationException"
        }
        ProviderError::Encode(EncodeError::Json(_)) => "System.Text.Json.JsonReaderException",
        ProviderError::Http(HttpError::InvalidJsonString(_)) => "System.InvalidOperationException",
        ProviderError::Provider(_)
        | ProviderError::Credential(_)
        | ProviderError::Http(HttpError::Provider(_)) => "Parrot.Llm.LLMProviderException",
        ProviderError::Catalog(_) => "System.Text.Json.JsonReaderException",
        ProviderError::StandardCatalog(error) => {
            use parrot_provider::catalogs::StandardModelError;
            match error {
                StandardModelError::Json(_) => "System.Text.Json.JsonReaderException",
                StandardModelError::InvalidOperation(_) => "System.InvalidOperationException",
                StandardModelError::Provider(_) => "Parrot.Llm.LLMProviderException",
            }
        }
        ProviderError::Authentication(error) => {
            use parrot_foundation::oauth::AuthError;
            match error {
                AuthError::Message(_)
                | AuthError::Http { .. }
                | AuthError::Credential(_)
                | AuthError::BrowserLaunch(_) => "Parrot.Auth.AuthException",
                AuthError::Transport | AuthError::TransportWithMessage(_) => {
                    "System.Net.Http.HttpRequestException"
                }
                AuthError::Io | AuthError::IoWithMessage(_) => "System.IO.IOException",
                AuthError::InvalidOperation(_) => "System.InvalidOperationException",
                AuthError::Format | AuthError::FormatWithMessage(_) => "System.FormatException",
                AuthError::ArgumentOutOfRange(_)
                | AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                    "System.ArgumentOutOfRangeException"
                }
                AuthError::NotSupported(_) => "System.NotSupportedException",
            }
        }
        ProviderError::Http(_) => {
            result["status"] = json!(0);
            result["type"] = json!("");
            result["code"] = json!("");
            result["detail"] = json!("");
            result["response_body"] = json!("");
            "Parrot.Llm.Wire.ProviderHttpException"
        }
        ProviderError::Encode(EncodeError::Image(_)) => "System.IO.IOException",
    };
    result["category"] = json!(category);
    result
}
fn publish(events: &mut Vec<Value>, value: Value, token: &Token, cancel_after: i64) {
    events.push(value);
    if events.len() as i64 == cancel_after {
        token.cancel();
    }
}

pub async fn observe(case_id: &str, input: &Value) -> Value {
    let (mut observed, raw_encoded_json) = observe_inner(case_id, input).await;
    if let Some(raw) = raw_encoded_json {
        observed["encoded_json"] = serde_json::from_str(&raw)
            .expect("v1 observations contain finite, uniquely named JSON values");
    }
    observed
}

pub async fn observe_raw(case_id: &str, input: &Value) -> String {
    let (observed, raw_encoded_json) = observe_inner(case_id, input).await;
    let properties = observed
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            let value = if key == "encoded_json" {
                raw_encoded_json.as_deref().unwrap_or("null").to_owned()
            } else {
                wire_json::serialize(value)
            };
            format!("{}:{value}", wire_json::serialize(&json!(key)))
        })
        .collect::<Vec<_>>();
    format!("{{{}}}", properties.join(","))
}

/// The raw producer never loads observations. Its output includes native stdout's newline.
pub async fn produce_raw(inputs: &Value, contract: &str) -> String {
    let mut cases = Vec::new();
    for case in inputs["cases"].as_array().unwrap() {
        let id = case["case_id"].as_str().unwrap();
        let input = &case["input"];
        cases.push(format!(
            "{{\"case_id\":{},\"input\":{},\"observed\":{}}}",
            wire_json::serialize(&json!(id)),
            wire_json::serialize(input),
            observe_raw(id, input).await
        ));
    }
    format!(
        "{{\"source_commit\":\"5f27a8a92625ee31d8bec094e5c3e09550193781\",\"contract\":{},\"cases\":[{}]}}\n",
        wire_json::serialize(&json!(contract)),
        cases.join(",")
    )
}

async fn observe_inner(_case_id: &str, input: &Value) -> (Value, Option<String>) {
    let operation = s(input, "operation");
    let options = &input["options"];
    let token = Token::new();
    let disposed = Arc::new(AtomicBool::new(false));
    let body = Body {
        bytes: s(input, "sse").as_bytes().to_vec(),
        position: 0,
        reads: 0,
        chunks: input["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as usize)
            .collect(),
        disposed: disposed.clone(),
    };
    let key = Arc::new(Key {
        value: s(options, "api_key").into(),
        api: AtomicUsize::new(0),
        has: AtomicUsize::new(0),
    });
    let transport = Arc::new(Transport {
        body: Mutex::new(Some(body)),
        status: n(options, "status") as u16,
        content_type: s(options, "content_type").into(),
        requests: Mutex::new(Vec::new()),
    });
    let request = Request::new(&input["request"]);
    let cancel_after = n(input, "cancel_after_events");
    let mut observed = json!({"events":[],"error":null,"encoded_json":null,"encoded_hex":null,"requests":[],"stream_disposed":false,"api_key_calls":0,"has_credential_calls":0,"models":null,"has_credential":null});
    let mut events = Vec::new();
    let mut raw_encoded_json = None;
    let outcome: Result<(), ProviderError> = async {
        if operation == "encode" {
            let prepared = encode(&request, s(input, "provider_id"))?;
            observed["encoded_hex"] = json!(hex(&prepared.body));
            raw_encoded_json = Some(
                wire_json::normalize_raw_json(
                    std::str::from_utf8(&prepared.body).expect("encoded request is UTF-8"),
                )
                .expect("encoded request is valid raw JSON"),
            );
        } else if operation == "parse" {
            if cancel_after == 0 {
                token.cancel();
            }
            let body = transport.body.lock().unwrap().take().unwrap();
            parse(body, &token, input, &mut events, cancel_after).await?;
        } else {
            let mut config =
                AnthropicOptions::new(s(input, "provider_id"), s(options, "base_url"), key.clone());
            config.user_agent = "parrot/0.0.0-dev".into();
            config.headers = options["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
                .collect();
            config.maximum_request_bytes = n(options, "maximum_request_bytes") as usize;
            config.allow_insecure_localhost = b(options, "allow_insecure_localhost");
            config.allow_insecure_remote = b(options, "allow_insecure_remote");
            config.header_timeout_ms = n(options, "header_timeout_ms");
            config.stream_idle_timeout_ms = n(options, "stream_idle_timeout_ms");
            let provider = AnthropicProvider::new(config, transport.clone())?;
            if operation == "credential" {
                observed["has_credential"] = json!(provider.has_credential(&token).await?);
            } else if operation == "list" {
                observed["models"] = json!(
                    provider
                        .list_models(&token)
                        .await?
                        .into_iter()
                        .map(model)
                        .collect::<Vec<_>>()
                );
            } else {
                if cancel_after == 0 {
                    token.cancel();
                }
                let mut stream = provider.call(&request, &token).await?;
                while let Some(value) = stream.next(&token).await? {
                    let value = match value {
                        AnthropicStreamEvent::HttpRequestStarted => event("HttpRequestStarted"),
                        AnthropicStreamEvent::HttpResponseHeadersReceived => {
                            event("HttpResponseHeadersReceived")
                        }
                        AnthropicStreamEvent::Message(value) => message_event(value),
                    };
                    publish(&mut events, value, &token, cancel_after);
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(value) = outcome {
        observed["error"] = error(value, operation);
    }
    observed["events"] = json!(events);
    observed["requests"] = json!(*transport.requests.lock().unwrap());
    observed["stream_disposed"] = json!(disposed.load(Ordering::SeqCst));
    observed["api_key_calls"] = json!(key.api.load(Ordering::SeqCst));
    observed["has_credential_calls"] = json!(key.has.load(Ordering::SeqCst));
    (observed, raw_encoded_json)
}
async fn parse(
    body: Body,
    token: &Token,
    input: &Value,
    events: &mut Vec<Value>,
    cancel_after: i64,
) -> Result<(), ProviderError> {
    let mut stream = SseStream::new(body, n(input, "max_event_bytes") as i32);
    let mut parser = AnthropicParser::new(s(input, "provider_id"));
    while let Some(record) = stream.next(token).await? {
        for value in parser.consume(&record.data)? {
            publish(events, message_event(value), token, cancel_after);
        }
        if parser.is_stopped() {
            while let Some(value) = parser.finish_next()? {
                publish(events, message_event(value), token, cancel_after);
            }
            return Ok(());
        }
    }
    while let Some(value) = parser.finish_next()? {
        publish(events, message_event(value), token, cancel_after);
    }
    Ok(())
}

#[test]
fn authentication_projection_uses_source_exception_categories() {
    use parrot_foundation::{credentials::CredentialError, oauth::AuthError};
    for (original, category) in [
        (AuthError::Message("synthetic"), "Parrot.Auth.AuthException"),
        (
            AuthError::Http {
                operation: "synthetic",
                status: 401,
            },
            "Parrot.Auth.AuthException",
        ),
        (
            AuthError::Credential(CredentialError::NotFound),
            "Parrot.Auth.AuthException",
        ),
        (AuthError::BrowserLaunch(None), "Parrot.Auth.AuthException"),
        (AuthError::Transport, "System.Net.Http.HttpRequestException"),
        (AuthError::Io, "System.IO.IOException"),
        (
            AuthError::InvalidOperation("synthetic"),
            "System.InvalidOperationException",
        ),
        (AuthError::Format, "System.FormatException"),
        (
            AuthError::ArgumentOutOfRange("synthetic"),
            "System.ArgumentOutOfRangeException",
        ),
        (
            AuthError::NotSupported("synthetic"),
            "System.NotSupportedException",
        ),
    ] {
        let message = original.to_string();
        let observed = error(ProviderError::Authentication(original), "call");
        assert_eq!(observed["category"], category);
        assert_eq!(observed["message"], message);
    }
}
