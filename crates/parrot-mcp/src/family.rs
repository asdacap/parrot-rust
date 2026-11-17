//! MCP tool family value types. The outcome is the caller-visible text of one
//! call; the error carries the failure category the composition host maps onto
//! settlement failure kinds.

use std::fmt;

/// Text result of one MCP tool call. The text is the caller-visible content;
/// `is_error` marks a result the server itself reported as failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpToolOutcome {
    pub text: String,
    pub is_error: bool,
}

/// Failure to reach or complete an MCP tool call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpToolError {
    pub server: String,
    pub kind: McpToolFailureKind,
    pub reason: String,
}

impl McpToolError {
    pub fn new(
        server: impl Into<String>,
        kind: McpToolFailureKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            server: server.into(),
            kind,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for McpToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "mcp server '{}' is unavailable: {}",
            self.server, self.reason
        )
    }
}

impl std::error::Error for McpToolError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpToolFailureKind {
    /// The named server is not connected or failed to connect.
    Unavailable,
    /// The server ended the connection during the call.
    Closed,
    /// The call exceeded the server's timeout.
    Timeout,
    /// The transport or protocol failed.
    Transport,
    /// The call arguments could not be parsed.
    Arguments,
}
