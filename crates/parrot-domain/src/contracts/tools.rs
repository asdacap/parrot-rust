//! Tool invocation boundaries; concrete tools and scheduling belong to later tasks.

use std::fmt;

use super::async_ops::{BoxFuture, OperationError};
use super::security::{AccessSnapshot, InvocationScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelSafety {
    Sequential,
    ParallelSafe,
}

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

/// Calls tools on the user session's configured MCP servers. Implementations
/// own the connection lifetime; see the `mcp:` configuration contract.
pub trait McpToolCaller: Send + Sync {
    /// Calls one tool on one named server. `arguments_json` is forwarded to
    /// the server verbatim as the `tools/call` arguments object.
    fn call_mcp_tool<'a>(
        &'a self,
        server: String,
        tool: String,
        arguments_json: String,
    ) -> BoxFuture<'a, Result<McpToolOutcome, McpToolError>>;
}

/// No default parallel safety or default successful execution. T47 captures
/// security at every invocation start and keeps ordered results across contiguous
/// parallel-safe batches; it must not hold blocking guards through await.
pub trait Tool: Send + Sync {
    type Definition: Send + Sync + 'static;
    type Invocation: Send + 'static;
    type Selection: Send + 'static;
    type Security: AccessSnapshot + 'static;
    type Output: Send + 'static;
    type Error: Send + Sync + 'static;

    fn definition(&self) -> &Self::Definition;

    /// Safety is invocation-dependent, not a single unconditional tool flag.
    fn parallel_safety(&self, invocation: &Self::Invocation) -> ParallelSafety;

    /// Explicitly declares whether this tool may run after turn interruption.
    fn enabled_after_interruption(&self) -> bool;

    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        scope: InvocationScope<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, Result<Self::Output, OperationError<Self::Error>>>;
}
