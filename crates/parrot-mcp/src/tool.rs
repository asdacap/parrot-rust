//! The MCP tool family as it appears in the agent-session tool catalog. The
//! tool satisfies the domain `Tool` contract with its own types; composition
//! binds it into the host catalog.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_foundation::template_catalog::PromptTemplateCatalog;

use crate::family::{McpToolError, McpToolFailureKind, McpToolOutcome};
use crate::manager::McpServerManager;

/// What one MCP tool invocation consumes. The host catalog hands these fields
/// over; the tool forwards `arguments_json` verbatim.
pub struct McpInvocation {
    pub call_id: String,
    pub arguments_json: String,
    pub prompt_templates: Arc<PromptTemplateCatalog>,
}

impl McpInvocation {
    pub fn new(
        call_id: impl Into<String>,
        arguments_json: impl Into<String>,
        prompt_templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            arguments_json: arguments_json.into(),
            prompt_templates,
        }
    }
}

pub struct McpTool<S> {
    manager: Arc<McpServerManager>,
    server: String,
    tool: String,
    definition: ConfiguredToolDefinition,
    marker: PhantomData<fn() -> S>,
}

impl<S: AccessSnapshot + Clone + 'static> Tool for McpTool<S> {
    type Definition = ConfiguredToolDefinition;
    type Invocation = McpInvocation;
    type Selection = ();
    type Security = S;
    type Output = McpToolOutcome;
    type Error = McpToolError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }

    fn parallel_safety(&self, _invocation: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }

    fn enabled_after_interruption(&self) -> bool {
        false
    }

    fn invoke<'a>(
        &'a self,
        invocation: McpInvocation,
        scope: InvocationScope<'a, S, ()>,
    ) -> BoxFuture<'a, Result<McpToolOutcome, OperationError<McpToolError>>> {
        Box::pin(async move {
            let outcome = self
                .manager
                .call_tool(&self.server, &self.tool, &invocation.arguments_json)
                .await;
            match outcome {
                Ok(outcome) if !outcome.is_error => Ok(outcome),
                Ok(outcome) => {
                    let text = invocation
                        .prompt_templates
                        .render(
                            "tool-result.error",
                            &[(
                                "message".to_owned(),
                                serde_json::Value::String(outcome.text),
                            )],
                            scope.cancellation(),
                        )
                        .map_err(|error| match error {
                            OperationError::Cancelled => OperationError::Cancelled,
                            OperationError::Failed(error) => {
                                OperationError::Failed(McpToolError::new(
                                    &self.server,
                                    McpToolFailureKind::Transport,
                                    error.to_string(),
                                ))
                            }
                        })?;
                    Ok(McpToolOutcome {
                        text,
                        is_error: true,
                    })
                }
                Err(error) => Err(OperationError::Failed(error)),
            }
        })
    }
}

/// One live MCP tool as composition receives it: the namespaced catalog name
/// plus the ready tool instance.
pub struct McpToolEntry<S> {
    pub name: String,
    pub tool: McpTool<S>,
}

/// Per-tool factory inputs for the host catalog merge point.
pub struct McpFactories<S>(PhantomData<fn() -> S>);

impl<S: AccessSnapshot + Clone + 'static> McpFactories<S> {
    /// One entry per live tool, named `mcp__<server>__<tool>`. Definitions come
    /// from the manager's `tools/list` snapshot, not from the config catalog.
    pub fn tools(
        manager: Arc<McpServerManager>,
        connected: &BTreeMap<String, BTreeMap<String, ConfiguredToolDefinition>>,
    ) -> Vec<McpToolEntry<S>> {
        connected
            .iter()
            .flat_map(|(server, tools)| {
                let manager = manager.clone();
                tools.iter().map(move |(tool_name, definition)| {
                    let tool = McpTool {
                        manager: manager.clone(),
                        server: server.clone(),
                        tool: tool_name.clone(),
                        definition: definition.clone(),
                        marker: PhantomData,
                    };
                    McpToolEntry {
                        name: format!("mcp__{server}__{tool_name}"),
                        tool,
                    }
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::testing::stdio_server;
    use parrot_domain::contracts::async_ops::Cancellation;
    use parrot_domain::contracts::paths::ResolvedRead;
    use std::future::pending;

    struct NoCancel;
    impl Cancellation for NoCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(pending())
        }
    }

    #[derive(Clone)]
    struct TestSecurity;
    impl AccessSnapshot for TestSecurity {
        type Error = std::io::Error;
        fn check_read(&self, _read: &ResolvedRead) -> Result<(), Self::Error> {
            Ok(())
        }
        fn check_write(&self, _target: &std::path::Path) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn templates() -> Arc<PromptTemplateCatalog> {
        Arc::new(
            PromptTemplateCatalog::from_content(
                "",
                &BTreeMap::from([("HOME".into(), "/synthetic/mcp-home".into())]),
            )
            .unwrap()
            .1,
        )
    }

    async fn fixture_entry() -> (Arc<McpServerManager>, McpToolEntry<TestSecurity>) {
        let manager = McpServerManager::connect(&BTreeMap::from([(
            "fixture".to_owned(),
            stdio_server("sh", true),
        )]))
        .await;
        let entry =
            McpFactories::<TestSecurity>::tools(manager.clone(), &manager.connected_tools())
                .remove(0);
        (manager, entry)
    }

    #[tokio::test]
    async fn entries_carry_namespaced_names_and_server_schemas() {
        let (manager, entry) = fixture_entry().await;
        assert_eq!(entry.name, "mcp__fixture__echo");
        let definition = entry.tool.definition();
        assert_eq!(definition.description, "Echo tool");
        assert_eq!(definition.parameters_json, "{\"type\":\"object\"}");
        assert_eq!(
            entry
                .tool
                .parallel_safety(&McpInvocation::new("call", "{}", templates())),
            ParallelSafety::Sequential
        );
        manager.close().await;
    }

    #[tokio::test]
    async fn invocation_forwards_arguments_verbatim_and_returns_text() {
        let (manager, entry) = fixture_entry().await;
        let outcome = entry
            .tool
            .invoke(
                McpInvocation::new("call-1", r#"{"text":"hi"}"#, templates()),
                InvocationScope::new(TestSecurity, (), &NoCancel),
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, r#"echo: {"text":"hi"}"#);
        assert!(!outcome.is_error);
        manager.close().await;
    }

    #[tokio::test]
    async fn server_error_results_render_through_the_error_template() {
        let (manager, entry) = fixture_entry().await;
        let outcome = entry
            .tool
            .invoke(
                McpInvocation::new("call-2", r#"{"text":"boom"}"#, templates()),
                InvocationScope::new(TestSecurity, (), &NoCancel),
            )
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.text.contains("boom"));
        manager.close().await;
    }
}
