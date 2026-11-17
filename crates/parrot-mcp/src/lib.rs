//! Minimal MCP client for Parrot: stdio and streamable-HTTP transports,
//! handshake, `tools/list`, and `tools/call`.

pub mod client;
pub mod family;
pub mod manager;
pub mod protocol;
pub mod tool;
pub mod transport;

#[cfg(test)]
mod tests;

pub use client::McpClient;
pub use family::{McpToolError, McpToolFailureKind, McpToolOutcome};
pub use manager::McpServerManager;
pub use protocol::{
    CallToolResult, ClientInfo, ContentBlock, Implementation, InitializeResult, ListToolsParams,
    ListToolsResult, McpError, PROTOCOL_VERSION, ProtocolError, ResourceContents, Tool,
};
pub use tool::{McpFactories, McpInvocation, McpTool, McpToolEntry};
pub use transport::{HttpServerParams, StdioServerParams};
