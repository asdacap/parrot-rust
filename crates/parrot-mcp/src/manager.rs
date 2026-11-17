//! Ownership of the configured MCP server connections.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_foundation::config::runtime::{McpServerConfig, McpTransport};
use parrot_tools::mcp::{McpToolError, McpToolFailureKind, McpToolOutcome};

use crate::client::McpClient;
use crate::protocol::{CallToolResult, ClientInfo, ContentBlock, McpError};
use tokio::sync::Mutex as AsyncMutex;

const STARTUP_TIMEOUT_DEFAULT_MS: i32 = 15000;
const CALL_TIMEOUT_DEFAULT_MS: i32 = 30000;

/// The timeout a zero config value maps to, per the template's "zero uses
/// default" rule.
fn timeout(configured_ms: i32, default_ms: i32) -> Duration {
    Duration::from_millis(if configured_ms > 0 {
        configured_ms as u64
    } else {
        default_ms as u64
    })
}

fn failure(server: &str, kind: McpToolFailureKind, error: &McpError) -> McpToolError {
    McpToolError::new(server, kind, error.to_string())
}

enum McpServerState {
    Connected {
        client: Arc<AsyncMutex<McpClient>>,
        call_timeout: Duration,
        tools: BTreeMap<String, ConfiguredToolDefinition>,
    },
    Failed {
        reason: String,
    },
}

impl McpServerState {
    fn tools(&self) -> Option<&BTreeMap<String, ConfiguredToolDefinition>> {
        match self {
            Self::Connected { tools, .. } if !tools.is_empty() => Some(tools),
            _ => None,
        }
    }
}

/// One connection per enabled `mcp:` server. A server that fails to connect is
/// recorded as failed; it holds no tools and its tool calls fail with the
/// recorded reason. The caller owns the connection lifetime.
pub struct McpServerManager {
    servers: StdMutex<BTreeMap<String, McpServerState>>,
}

impl McpServerManager {
    /// Connects and handshakes every enabled server, then snapshots its tool
    /// list. Bounded per server by `startup_timeout_ms`; zero uses the default.
    pub async fn connect(config: &BTreeMap<String, McpServerConfig>) -> Arc<Self> {
        let mut servers = BTreeMap::new();
        for (name, server) in config {
            if !server.enabled {
                continue;
            }
            let state = match Self::open(server).await {
                Ok(state) => state,
                Err(error) => McpServerState::Failed {
                    reason: error.to_string(),
                },
            };
            servers.insert(name.clone(), state);
        }
        Arc::new(Self {
            servers: StdMutex::new(servers),
        })
    }

    async fn open(server: &McpServerConfig) -> Result<McpServerState, McpError> {
        let startup = timeout(server.startup_timeout_ms, STARTUP_TIMEOUT_DEFAULT_MS);
        let mut client = match server.transport {
            McpTransport::Stdio => {
                McpClient::connect_stdio(&crate::transport::StdioServerParams {
                    command: server.command.clone().unwrap_or_default(),
                    args: server.args.clone(),
                    env: server.env.clone(),
                    cwd: server.cwd.clone(),
                })?
            }
            McpTransport::Http => McpClient::connect_http(&crate::transport::HttpServerParams {
                url: server.url.clone().unwrap_or_default(),
                headers: server.headers.clone(),
                allow_insecure_localhost: server.allow_insecure_localhost,
            })?,
        };
        client
            .initialize(ClientInfo::new("parrot", "0.0.0-dev"), startup)
            .await?;
        let call_timeout = timeout(server.call_timeout_ms, CALL_TIMEOUT_DEFAULT_MS);
        let listed = client.list_tools(call_timeout).await?;
        let mut tools = BTreeMap::new();
        for tool in listed {
            let parameters_json =
                serde_json::to_string(&tool.input_schema).unwrap_or_else(|_| "{}".to_owned());
            tools.insert(
                tool.name.clone(),
                ConfiguredToolDefinition {
                    description: tool.description.unwrap_or_default(),
                    parameters_json,
                },
            );
        }
        Ok(McpServerState::Connected {
            client: Arc::new(AsyncMutex::new(client)),
            call_timeout,
            tools,
        })
    }

    /// Every connected server with its tool definitions, keyed by server name.
    /// Failed, disabled, and tool-less servers are absent.
    pub fn connected_tools(&self) -> BTreeMap<String, BTreeMap<String, ConfiguredToolDefinition>> {
        self.servers
            .lock()
            .expect("mcp servers poisoned")
            .iter()
            .filter_map(|(name, state)| state.tools().map(|tools| (name.clone(), tools.clone())))
            .collect()
    }

    /// The servers that failed to connect, with the recorded reason.
    pub fn failures(&self) -> Vec<(String, String)> {
        self.servers
            .lock()
            .expect("mcp servers poisoned")
            .iter()
            .filter_map(|(name, state)| match state {
                McpServerState::Failed { reason } => Some((name.clone(), reason.clone())),
                McpServerState::Connected { .. } => None,
            })
            .collect()
    }

    /// Calls one tool on one server under the server's call timeout.
    /// `arguments_json` is forwarded to the server verbatim as the
    /// `tools/call` arguments object.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments_json: &str,
    ) -> Result<McpToolOutcome, McpToolError> {
        let arguments: serde_json::Value =
            serde_json::from_str(arguments_json).map_err(|error| {
                McpToolError::new(server, McpToolFailureKind::Arguments, error.to_string())
            })?;
        let result = self.call(server, tool, arguments).await?;
        Ok(McpToolOutcome {
            text: tool_text(&result),
            is_error: result.is_error,
        })
    }

    async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpToolError> {
        let (client, call_timeout) = {
            let servers = self.servers.lock().expect("mcp servers poisoned");
            match servers.get(server) {
                Some(McpServerState::Connected {
                    client,
                    call_timeout,
                    ..
                }) => (client.clone(), *call_timeout),
                Some(McpServerState::Failed { reason }) => {
                    return Err(McpToolError::new(
                        server,
                        McpToolFailureKind::Unavailable,
                        reason.clone(),
                    ));
                }
                None => {
                    return Err(McpToolError::new(
                        server,
                        McpToolFailureKind::Unavailable,
                        "the server is not connected",
                    ));
                }
            }
        };
        let mut client = client.lock().await;
        client
            .call_tool(tool, arguments, call_timeout)
            .await
            .map_err(|error| failure(server, transport_kind(&error), &error))
    }

    /// Ends every connection, terminating server processes and HTTP sessions.
    pub async fn close(&self) {
        let servers = {
            let mut servers = self.servers.lock().expect("mcp servers poisoned");
            std::mem::take(&mut *servers)
        };
        for (_, state) in servers {
            if let McpServerState::Connected { client, .. } = state {
                client.lock().await.close().await;
            }
        }
    }
}

fn transport_kind(error: &McpError) -> McpToolFailureKind {
    match error {
        McpError::Closed => McpToolFailureKind::Closed,
        McpError::Timeout => McpToolFailureKind::Timeout,
        McpError::Transport(_) | McpError::Protocol(_) | McpError::Server(_) => {
            McpToolFailureKind::Transport
        }
    }
}

/// The caller-visible text of one `tools/call` result. Image and resource
/// blocks cannot be shown; their count becomes a trailing note.
fn tool_text(result: &CallToolResult) -> String {
    let mut text = result.text();
    let unsupported = result
        .content
        .iter()
        .filter(|block| !matches!(block, ContentBlock::Text { .. }))
        .count();
    if unsupported > 0 {
        let plural = if unsupported == 1 { "block" } else { "blocks" };
        text.push_str(&format!(
            "\n[the result also contained {unsupported} image or resource {plural} that cannot be displayed]"
        ));
    }
    text
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use parrot_foundation::config::runtime::McpServerConfig;

    /// A fake stdio MCP server. `tools/call` arguments are echoed back as the
    /// result text; the argument text "boom" answers with `isError: true`.
    pub const FAKE_SERVER_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo tool","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"tools/call"'*)
      args=$(printf '%s' "$line" | sed -n 's/.*"arguments":\(.*\)}}$/\1/p')
      escaped=$(printf '%s' "$args" | sed 's/"/\\"/g')
      case "$args" in
        *'"boom"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"boom"}],"isError":true}}\n' "$id" ;;
        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}],"isError":false}}\n' "$id" "$escaped" ;;
      esac ;;
  esac
done
"#;

    pub fn stdio_server(command: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Stdio,
            command: Some(command.to_owned()),
            args: vec!["-c".to_owned(), FAKE_SERVER_SCRIPT.to_owned()],
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled,
            allow_insecure_localhost: false,
            startup_timeout_ms: 10000,
            call_timeout_ms: 10000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::stdio_server;
    use super::*;
    use parrot_foundation::config::runtime::{McpServerConfig, McpTransport};
    use parrot_tools::mcp::McpToolFailureKind;

    fn http_server(url: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some(url.to_owned()),
            headers: BTreeMap::new(),
            enabled,
            allow_insecure_localhost: true,
            startup_timeout_ms: 10000,
            call_timeout_ms: 10000,
        }
    }

    fn config_with(entries: &[(&str, McpServerConfig)]) -> BTreeMap<String, McpServerConfig> {
        entries
            .iter()
            .map(|(name, server)| ((*name).to_owned(), server.clone()))
            .collect()
    }

    async fn connect(entries: &[(&str, McpServerConfig)]) -> Arc<McpServerManager> {
        McpServerManager::connect(&config_with(entries)).await
    }

    #[tokio::test]
    async fn enabled_stdio_server_connects_and_lists_tools() {
        let manager = connect(&[("fake", stdio_server("sh", true))]).await;
        assert!(manager.failures().is_empty());
        let tools = manager.connected_tools();
        let echo = &tools["fake"]["echo"];
        assert_eq!(echo.description, "Echo tool");
        assert_eq!(echo.parameters_json, "{\"type\":\"object\"}");
        manager.close().await;
        assert!(manager.connected_tools().is_empty());
    }

    #[tokio::test]
    async fn tool_call_forwards_arguments_verbatim_and_returns_text() {
        let manager = connect(&[("fake", stdio_server("sh", true))]).await;
        let outcome = manager
            .call_tool("fake", "echo", r#"{"text":"hi"}"#)
            .await
            .expect("the fake server answers the call");
        assert!(!outcome.is_error);
        assert_eq!(outcome.text, r#"echo: {"text":"hi"}"#);
        manager.close().await;
    }

    #[tokio::test]
    async fn invalid_arguments_fail_without_reaching_the_server() {
        let manager = connect(&[("fake", stdio_server("sh", true))]).await;
        let error = manager
            .call_tool("fake", "echo", "not json")
            .await
            .expect_err("invalid arguments fail locally");
        assert_eq!(error.server, "fake");
        assert_eq!(error.kind, McpToolFailureKind::Arguments);
        manager.close().await;
    }

    #[tokio::test]
    async fn bad_command_records_failure_without_blocking() {
        let manager = connect(&[("bad", stdio_server("/nonexistent/mcp-server", true))]).await;
        let failures = manager.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "bad");
        assert!(manager.connected_tools().is_empty());
        let error = manager
            .call_tool("bad", "echo", "{}")
            .await
            .expect_err("a failed server refuses calls");
        assert_eq!(error.server, "bad");
        manager.close().await;
    }

    #[tokio::test]
    async fn disabled_and_absent_servers_stay_unavailable() {
        let manager = connect(&[
            ("off", stdio_server("sh", false)),
            ("remote", http_server("http://127.0.0.1:9/mcp", true)),
        ])
        .await;
        assert_eq!(
            manager
                .failures()
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["remote"]
        );
        for name in ["off", "remote", "unknown"] {
            let error = manager
                .call_tool(name, "echo", "{}")
                .await
                .expect_err("an unconnected server refuses calls");
            assert_eq!(error.server, name);
            assert!(!error.reason.is_empty());
        }
        manager.close().await;
    }
}
