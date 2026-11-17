//! Line-delimited JSON over a child process and streamable HTTP, the two
//! transports an MCP server can use.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::protocol::{IncomingMessage, McpError};

const MCP_SESSION_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// Launch configuration of one stdio MCP server.
#[derive(Clone, Debug, Default)]
pub struct StdioServerParams {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// Connection configuration of one streamable-HTTP MCP server.
#[derive(Clone, Debug, Default)]
pub struct HttpServerParams {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub allow_insecure_localhost: bool,
}

/// One live connection to an MCP server. Requests run one at a time, so all
/// methods take `&mut self`.
pub enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport {
    /// Sends one request and returns the JSON-RPC response body the server
    /// produced for it. Server notifications are skipped for stdio.
    pub async fn request(&mut self, id: u64, message: &str) -> Result<String, McpError> {
        match self {
            Self::Stdio(transport) => transport.request(id, message).await,
            Self::Http(transport) => transport.request(message).await,
        }
    }

    /// Sends one notification that expects no response.
    pub async fn notify(&mut self, message: &str) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.notify(message).await,
            Self::Http(transport) => transport.notify(message).await,
        }
    }

    /// Records the negotiated protocol version for the HTTP header the spec
    /// asks clients to send after the handshake.
    pub fn set_protocol_version(&mut self, version: &str) {
        if let Self::Http(transport) = self {
            transport.protocol_version = Some(version.to_owned());
        }
    }

    /// Closes the connection. For stdio this kills the child and reaps it; for
    /// HTTP it asks the server to end the session and ignores failures.
    pub async fn close(&mut self) {
        match self {
            Self::Stdio(transport) => transport.close().await,
            Self::Http(transport) => transport.close().await,
        }
    }
}

/// A child process speaking line-delimited JSON on stdin and stdout.
pub struct StdioTransport {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
}

impl StdioTransport {
    pub fn spawn(params: &StdioServerParams) -> Result<Self, McpError> {
        let mut command = Command::new(&params.command);
        command
            .args(&params.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.envs(&params.env);
        if let Some(cwd) = &params.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|error| McpError::Transport(error.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".into()))?;
        // The server writes diagnostics to stderr; drain it so the child never
        // blocks on a full pipe.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr);
                let mut scratch = String::new();
                loop {
                    scratch.clear();
                    match lines.read_line(&mut scratch).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
        Ok(Self {
            stdin,
            stdout: BufReader::new(stdout),
            child,
        })
    }

    async fn write(&mut self, message: &str) -> Result<(), McpError> {
        self.stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))
    }

    async fn request(&mut self, id: u64, message: &str) -> Result<String, McpError> {
        self.write(message).await?;
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|error| McpError::Transport(error.to_string()))?;
            if read == 0 {
                return Err(McpError::Closed);
            }
            let incoming = IncomingMessage::parse(line.trim())?;
            // Notifications carry no id; the next response is the one the
            // sequential client is waiting for.
            if incoming.method.is_some() && incoming.id.is_none() {
                continue;
            }
            match incoming.id {
                Some(answer) if answer == id => return Ok(line.trim().to_owned()),
                _ => return Err(McpError::Protocol("response id mismatch".into())),
            }
        }
    }

    async fn notify(&mut self, message: &str) -> Result<(), McpError> {
        self.write(message).await
    }

    async fn close(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// A streamable-HTTP session. Each request is one POST; the server answers
/// with a single JSON document or an SSE stream holding one response event.
pub struct HttpTransport {
    client: reqwest::Client,
    url: reqwest::Url,
    headers: HeaderMap,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl HttpTransport {
    pub fn new(params: &HttpServerParams) -> Result<Self, McpError> {
        let url = reqwest::Url::parse(&params.url)
            .map_err(|error| McpError::Transport(error.to_string()))?;
        let loopback = url
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "[::1]");
        if url.scheme() != "https" && !(params.allow_insecure_localhost && loopback) {
            return Err(McpError::Transport(
                "plain http is allowed only for localhost endpoints".into(),
            ));
        }
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if params.allow_insecure_localhost && loopback {
            builder = builder
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }
        let client = builder
            .build()
            .map_err(|error| McpError::Transport(error.to_string()))?;
        let mut headers = HeaderMap::new();
        for (name, value) in &params.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| McpError::Transport(error.to_string()))?;
            let value = HeaderValue::from_str(value)
                .map_err(|error| McpError::Transport(error.to_string()))?;
            headers.insert(name, value);
        }
        Ok(Self {
            client,
            url,
            headers,
            session_id: None,
            protocol_version: None,
        })
    }

    async fn post(&mut self, message: &str) -> Result<reqwest::Response, McpError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .headers(self.headers.clone());
        if let Some(session) = &self.session_id {
            request = request.header(MCP_SESSION_HEADER, session);
        }
        if let Some(version) = &self.protocol_version {
            request = request.header(MCP_PROTOCOL_HEADER, version);
        }
        request
            .body(message.to_owned())
            .send()
            .await
            .map_err(Into::into)
    }

    async fn request(&mut self, message: &str) -> Result<String, McpError> {
        let response = self.post(message).await?;
        if let Some(session) = response
            .headers()
            .get(MCP_SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session.to_owned());
        }
        let status = response.status();
        if !status.is_success() {
            return Err(McpError::Transport(format!("server answered {status}")));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response
            .text()
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        if content_type.contains("text/event-stream") {
            return extract_event_data(&body);
        }
        Ok(body)
    }

    async fn notify(&mut self, message: &str) -> Result<(), McpError> {
        self.post(message).await?;
        Ok(())
    }

    async fn close(&mut self) {
        // The DELETE request is a courtesy; a dropped client already ends the
        // pooled connections.
        if self.session_id.is_some() {
            let mut request = self
                .client
                .delete(self.url.clone())
                .headers(self.headers.clone());
            if let Some(session) = &self.session_id {
                request = request.header(MCP_SESSION_HEADER, session);
            }
            if let Some(version) = &self.protocol_version {
                request = request.header(MCP_PROTOCOL_HEADER, version);
            }
            let _ = tokio::time::timeout(Duration::from_millis(250), request.send()).await;
        }
    }
}

/// Reads the `data:` payload of the first event that carries JSON-RPC content.
fn extract_event_data(body: &str) -> Result<String, McpError> {
    for block in body.split("\n\n") {
        let mut data = Vec::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim_start());
            }
        }
        if !data.is_empty() {
            return Ok(data.join("\n"));
        }
    }
    Err(McpError::Protocol("event stream held no data".into()))
}

/// Parses one raw JSON-RPC response body into its result or error.
pub(crate) fn decode_response(body: &str, id: u64) -> Result<Value, McpError> {
    let incoming = IncomingMessage::parse(body.trim())?;
    match incoming.id {
        Some(answer) if answer == id => match (incoming.result, incoming.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(McpError::Server(error)),
            (None, None) => Err(McpError::Protocol(
                "response held neither result nor error".into(),
            )),
        },
        _ => Err(McpError::Protocol("response id mismatch".into())),
    }
}
