use serde::Deserialize;
use serde_json::value::RawValue;

/// Borrowed, adapter-local view into the caller-owned provider request. T30's
/// request history implements these views; no provider-neutral serialized DTO
/// is introduced here.
pub trait RequestView {
    type Message: MessageView;
    type Tool: ToolView;
    fn model(&self) -> &str;
    fn instructions(&self) -> &str;
    fn messages(&self) -> &[Self::Message];
    fn tools(&self) -> &[Self::Tool];
    fn reasoning_effort(&self) -> Option<&str>;
    fn reasoning_summary(&self) -> Option<&str>;
    fn provider_preferences(&self) -> &str;
    fn include_router_metadata(&self) -> bool;
    fn max_output_tokens(&self) -> i32;
}

pub trait MessageView {
    type Content: ContentView;
    type Call: ToolCallView;
    fn role(&self) -> MessageRole;
    fn text(&self) -> &str;
    fn contents(&self) -> &[Self::Content];
    fn tool_call_id(&self) -> &str;
    fn tool_calls(&self) -> &[Self::Call];
    fn provider_id(&self) -> &str;
    fn provider_content(&self) -> &str;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    System,
    Assistant,
    Tool,
    User,
}

pub trait ContentView {
    fn is_text(&self) -> bool;
    fn text(&self) -> &str;
    fn image_url(&self) -> &str;
    fn media_type(&self) -> &str;
    /// Reads only when encoding needs a data URL; ownership stays with the view.
    fn read_image(&self) -> Result<Vec<u8>, RequestError>;
}
pub trait ToolCallView {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn arguments_json(&self) -> &str;
}
pub trait ToolView {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_json(&self) -> &str;
}

/// Fully owned protocol request bytes prepared for the HTTP transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
}

pub fn prepare(
    request: &impl RequestView,
    provider_id: &str,
) -> Result<PreparedRequest, RequestError> {
    let mut body = String::from("{\"model\":");
    quoted(&mut body, request.model())?;
    body.push_str(",\"messages\":[");
    let mut first = true;
    if !request.instructions().is_empty() {
        body.push_str("{\"role\":\"system\",\"content\":");
        quoted(&mut body, request.instructions())?;
        body.push('}');
        first = false;
    }
    for message in request.messages() {
        if !first {
            body.push(',');
        }
        let has_previous_message = !first;
        first = false;
        write_message(&mut body, message, provider_id, has_previous_message)?;
    }
    body.push(']');
    if !request.tools().is_empty() {
        body.push_str(",\"tools\":[");
        for (index, tool) in request.tools().iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            body.push_str("{\"type\":\"function\",\"function\":{\"name\":");
            quoted(&mut body, tool.name())?;
            if !tool.description().is_empty() {
                body.push_str(",\"description\":");
                quoted(&mut body, tool.description())?;
            }
            body.push_str(",\"parameters\":");
            let schema = if tool.parameters_json().is_empty() {
                "{}"
            } else {
                tool.parameters_json()
            };
            raw_json(schema, false)?;
            body.push_str(schema);
            body.push_str("}}");
        }
        body.push(']');
    }
    body.push_str(",\"stream\":true,\"stream_options\":{\"include_usage\":true}");
    if let Some(effort) = request
        .reasoning_effort()
        .filter(|effort| !effort.is_empty())
    {
        body.push_str(",\"reasoning_effort\":");
        quoted(&mut body, effort)?;
    }
    let preferences = request.provider_preferences().trim();
    if !preferences.is_empty() {
        match raw_json(preferences, true) {
            Ok(()) => {}
            Err(RequestError::InvalidSchema(_)) => {
                return Err(RequestError::InvalidProviderPreferences);
            }
            Err(error) => return Err(error),
        }
        body.push_str(",\"provider\":");
        body.push_str(preferences);
    }
    if request.include_router_metadata() {
        body.push_str(",\"include_router_metadata\":true");
    }
    if request.max_output_tokens() > 0 {
        body.push_str(",\"max_tokens\":");
        body.push_str(&request.max_output_tokens().to_string());
    }
    body.push('}');
    Ok(PreparedRequest {
        body: body.into_bytes(),
    })
}

fn write_message(
    body: &mut String,
    message: &impl MessageView,
    provider_id: &str,
    has_previous_message: bool,
) -> Result<(), RequestError> {
    let replay = read_replay(message, provider_id);
    let nonleading_system = message.role() == MessageRole::System && has_previous_message;
    body.push('{');
    body.push_str("\"role\":");
    quoted(
        body,
        if nonleading_system {
            "user"
        } else {
            role_name(message.role())
        },
    )?;
    body.push_str(",\"content\":");
    if nonleading_system {
        quoted(body, &wrap_system_update(message.text()))?;
    } else if message.contents().iter().all(|part| part.is_text()) {
        if message.text().is_empty() && !message.tool_calls().is_empty() {
            body.truncate(body.len() - ",\"content\":".len());
        } else {
            quoted(body, message.text())?;
        }
    } else {
        body.push('[');
        for (index, part) in message.contents().iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            if part.is_text() {
                body.push_str("{\"type\":\"text\",\"text\":");
                quoted(body, part.text())?;
                body.push('}');
            } else {
                body.push_str("{\"type\":\"image_url\",\"image_url\":{\"url\":");
                quoted(body, &image_reference(part)?)?;
                body.push_str("}}");
            }
        }
        body.push(']');
    }
    if let Some(replay) = replay {
        if let Some(text) = replay.reasoning_content.as_deref() {
            body.push_str(",\"reasoning_content\":");
            quoted(body, text)?;
        }
        if let Some(text) = replay.reasoning.as_deref() {
            body.push_str(",\"reasoning\":");
            quoted(body, text)?;
        }
    }
    if !message.tool_call_id().is_empty() {
        body.push_str(",\"tool_call_id\":");
        quoted(body, message.tool_call_id())?;
    }
    if !message.tool_calls().is_empty() {
        body.push_str(",\"tool_calls\":[");
        for (index, call) in message.tool_calls().iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            body.push_str("{\"id\":");
            quoted(body, call.id())?;
            body.push_str(",\"type\":\"function\",\"function\":{\"name\":");
            quoted(body, call.name())?;
            body.push_str(",\"arguments\":");
            quoted(body, call.arguments_json())?;
            body.push_str("}}");
        }
        body.push(']');
    }
    body.push('}');
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReasoningReplay {
    version: i64,
    protocol: String,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
}
fn read_replay(message: &impl MessageView, provider_id: &str) -> Option<ReasoningReplay> {
    if message.role() != MessageRole::Assistant || message.provider_id() != provider_id {
        return None;
    }
    let fields: std::collections::BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(message.provider_content()).ok()?;
    let version = serde_json::from_str(fields.get("Version")?.get()).ok()?;
    let protocol = serde_json::from_str(fields.get("Protocol")?.get()).ok()?;
    let reasoning_content = fields
        .get("ReasoningContent")
        .map(|raw| serde_json::from_str::<Option<String>>(raw.get()))
        .transpose()
        .ok()?
        .flatten();
    let reasoning = fields
        .get("Reasoning")
        .map(|raw| serde_json::from_str::<Option<String>>(raw.get()))
        .transpose()
        .ok()?
        .flatten();
    let replay = ReasoningReplay {
        version,
        protocol,
        reasoning_content,
        reasoning,
    };
    (replay.version == 1 && replay.protocol == "chat-completions").then_some(replay)
}
fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
        MessageRole::User => "user",
    }
}
fn image_reference(content: &impl ContentView) -> Result<String, RequestError> {
    if !content.image_url().is_empty() {
        return Ok(content.image_url().to_owned());
    }
    Ok(format!(
        "data:{};base64,{}",
        content.media_type(),
        base64(&content.read_image()?)
    ))
}
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(ALPHABET[(a >> 2) as usize] as char);
        output.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
fn wrap_system_update(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    format!("<system-update>\n{escaped}\n</system-update>")
}
fn quoted(output: &mut String, text: &str) -> Result<(), RequestError> {
    output.push_str(
        &serde_json::to_string(text)
            .map_err(|error| RequestError::Serialize(error.to_string()))?
            .replace('<', "\\u003C")
            .replace('>', "\\u003E")
            .replace('&', "\\u0026"),
    );
    Ok(())
}
fn raw_json(text: &str, object_only: bool) -> Result<(), RequestError> {
    let raw: Box<RawValue> = serde_json::from_str(text).map_err(|error| {
        let message = if error.is_eof() && error.line() == 1 && error.column() == 1 {
            "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed. LineNumber: 0 | BytePositionInLine: 1.".to_owned()
        } else {
            error.to_string()
        };
        RequestError::InvalidSchema(message)
    })?;
    if object_only && !raw.get().trim_start().starts_with('{') {
        return Err(RequestError::ProviderPreferencesMustBeObject);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestError {
    InvalidSchema(String),
    InvalidProviderPreferences,
    ProviderPreferencesMustBeObject,
    Serialize(String),
    Image(String),
}
impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchema(error) => write!(f, "invalid tool schema: {error}"),
            Self::InvalidProviderPreferences => {
                f.write_str("provider preferences are not valid JSON")
            }
            Self::ProviderPreferencesMustBeObject => {
                f.write_str("provider preferences must be a JSON object")
            }
            Self::Serialize(error) | Self::Image(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for RequestError {}
