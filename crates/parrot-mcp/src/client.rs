//! Client operations on top of a transport: handshake, tool listing, and tool
//! calls. One request runs at a time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::time;

use crate::protocol::{
    self, CallToolParams, CallToolResult, ClientInfo, InitializeResult, ListToolsParams,
    ListToolsResult, McpError, Tool, encode_notification, encode_params, encode_request,
};
use crate::transport::{
    HttpServerParams, HttpTransport, StdioServerParams, StdioTransport, Transport, decode_response,
};

const INITIALIZED_NOTIFICATION: &str = "notifications/initialized";

/// One connected MCP client over stdio or streamable HTTP.
pub struct McpClient {
    transport: Transport,
    next_id: AtomicU64,
}

impl McpClient {
    /// Spawns the stdio server process. The handshake is separate.
    pub fn connect_stdio(params: &StdioServerParams) -> Result<Self, McpError> {
        Ok(Self {
            transport: Transport::Stdio(StdioTransport::spawn(params)?),
            next_id: AtomicU64::new(1),
        })
    }

    /// Opens the streamable-HTTP session. The handshake is separate.
    pub fn connect_http(params: &HttpServerParams) -> Result<Self, McpError> {
        Ok(Self {
            transport: Transport::Http(HttpTransport::new(params)?),
            next_id: AtomicU64::new(1),
        })
    }

    /// Runs the initialize handshake and the `notifications/initialized`
    /// acknowledgement. `timeout` bounds the initialize request.
    pub async fn initialize(
        &mut self,
        client_info: ClientInfo,
        timeout: Duration,
    ) -> Result<InitializeResult, McpError> {
        let id = self.reserve_id();
        let params = encode_params(protocol::InitializeParams::new(client_info))?;
        let message = encode_request(id, "initialize", &params)?;
        let body = time::timeout(timeout, self.transport.request(id, &message))
            .await
            .map_err(|_| McpError::Timeout)??;
        let result = decode_response(&body, id)?;
        let handshake: InitializeResult = serde_json::from_value(result)
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        self.transport
            .set_protocol_version(&handshake.protocol_version);
        let notification = encode_notification(INITIALIZED_NOTIFICATION)?;
        self.transport.notify(&notification).await?;
        Ok(handshake)
    }

    /// Lists every tool the server exposes, following pagination cursors.
    /// `timeout` bounds each page request.
    pub async fn list_tools(&mut self, timeout: Duration) -> Result<Vec<Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        loop {
            let id = self.reserve_id();
            let params = encode_params(ListToolsParams::new(cursor.clone()))?;
            let message = encode_request(id, "tools/list", &params)?;
            let body = time::timeout(timeout, self.transport.request(id, &message))
                .await
                .map_err(|_| McpError::Timeout)??;
            let result = decode_response(&body, id)?;
            let page: ListToolsResult = serde_json::from_value(result)
                .map_err(|error| McpError::Protocol(error.to_string()))?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(tools),
            }
        }
    }

    /// Calls one tool. `arguments` must already be a JSON object and `timeout`
    /// bounds the call.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        let id = self.reserve_id();
        let params = encode_params(CallToolParams {
            name: name.to_owned(),
            arguments,
        })?;
        let message = encode_request(id, "tools/call", &params)?;
        let body = time::timeout(timeout, self.transport.request(id, &message))
            .await
            .map_err(|_| McpError::Timeout)??;
        let result = decode_response(&body, id)?;
        serde_json::from_value(result).map_err(|error| McpError::Protocol(error.to_string()))
    }

    /// Ends the transport, terminating the server process or HTTP session.
    pub async fn close(&mut self) {
        self.transport.close().await;
    }

    fn reserve_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}
