//! MCP client messages, one struct per wire shape. `serde_json::Value` appears
//! only where the protocol itself carries arbitrary JSON: tool input schemas,
//! tool call arguments, and unknown server extensions.
//!
//! See https://modelcontextprotocol.io/specification/2025-06-18.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Identifies Parrot to the server during the initialize handshake.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl ClientInfo {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: &'static str,
    capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

impl InitializeParams {
    pub(crate) fn new(client_info: ClientInfo) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities {},
            client_info,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ClientCapabilities {}

/// The server's answer to the initialize request. Server capability blocks the
/// tools-only client does not use are ignored on parse.
#[derive(Clone, Debug, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(rename = "serverInfo")]
    pub server_info: Option<Implementation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct Implementation {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Parameters of a `tools/list` request.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

impl ListToolsParams {
    pub fn new(cursor: Option<String>) -> Self {
        Self { cursor }
    }
}

/// One tool advertised by a server.
#[derive(Clone, Debug, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Result of `tools/list`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
}

/// Arguments of a `tools/call` request.
#[derive(Clone, Debug, Serialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Result of `tools/call`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

impl CallToolResult {
    /// Concatenated text of every text content block. The usual result the
    /// caller wants to show.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<&str>>()
            .join("\n")
    }
}

/// One content block of a `tools/call` result.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "image")]
    Image {
        #[serde(default)]
        data: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
    },
    #[serde(rename = "resource")]
    Resource {
        #[serde(default)]
        resource: Option<ResourceContents>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ResourceContents {
    Text {
        uri: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
        text: String,
    },
    Blob {
        uri: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
        blob: String,
    },
}

/// A JSON-RPC error returned by the server in place of a result.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsonRpcMessage<'a> {
    Request {
        jsonrpc: &'static str,
        id: u64,
        method: &'a str,
        params: &'a Value,
    },
    Notification {
        jsonrpc: &'static str,
        method: &'static str,
    },
}

/// Wire envelope of an incoming server message. `result` stays untyped because
/// the caller knows which method it sent and deserializes the matching struct.
#[derive(Debug, Deserialize)]
pub(crate) struct IncomingMessage {
    #[serde(default)]
    pub(crate) id: Option<u64>,
    #[serde(default)]
    pub(crate) result: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<ProtocolError>,
    #[serde(default)]
    pub(crate) method: Option<String>,
}

impl IncomingMessage {
    pub(crate) fn parse(line: &str) -> Result<Self, McpError> {
        serde_json::from_str(line).map_err(|error| McpError::Protocol(error.to_string()))
    }
}

/// Error of every MCP client operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpError {
    /// The server closed its side or the child process exited.
    Closed,
    /// The operation exceeded the caller-supplied timeout.
    Timeout,
    /// The transport failed to send or receive.
    Transport(String),
    /// The server sent a message that does not follow the protocol.
    Protocol(String),
    /// The server answered a request with a JSON-RPC error.
    Server(ProtocolError),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("mcp server closed the connection"),
            Self::Timeout => formatter.write_str("mcp request timed out"),
            Self::Transport(message) => write!(formatter, "mcp transport failed: {message}"),
            Self::Protocol(message) => write!(formatter, "mcp protocol error: {message}"),
            Self::Server(error) => {
                write!(
                    formatter,
                    "mcp server error {}: {}",
                    error.code, error.message
                )
            }
        }
    }
}

impl std::error::Error for McpError {}

impl From<reqwest::Error> for McpError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

pub(crate) fn encode_request(id: u64, method: &str, params: &Value) -> Result<String, McpError> {
    serde_json::to_string(&JsonRpcMessage::Request {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .map_err(|error| McpError::Protocol(error.to_string()))
}

pub(crate) fn encode_notification(method: &'static str) -> Result<String, McpError> {
    serde_json::to_string(&JsonRpcMessage::Notification {
        jsonrpc: "2.0",
        method,
    })
    .map_err(|error| McpError::Protocol(error.to_string()))
}

pub(crate) fn encode_params(params: impl Serialize) -> Result<Value, McpError> {
    serde_json::to_value(&params).map_err(|error| McpError::Protocol(error.to_string()))
}
