//! Minimal MCP client for Parrot: stdio and streamable-HTTP transports,
//! handshake, `tools/list`, and `tools/call`.

pub mod client;
pub mod protocol;
pub mod transport;

#[cfg(test)]
mod tests;

pub use client::McpClient;
pub use protocol::{
    CallToolResult, ClientInfo, ContentBlock, Implementation, InitializeResult, ListToolsParams,
    ListToolsResult, McpError, PROTOCOL_VERSION, ProtocolError, ResourceContents, Tool,
};
pub use transport::{HttpServerParams, StdioServerParams};
