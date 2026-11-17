//! The MCP tool family as it appears in the agent-session tool catalog.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_tools::catalog;
use parrot_tools::mcp::{McpToolError, McpToolFailureKind};

use crate::manager::McpServerManager;

pub struct McpTool<Security, Selection> {
    manager: Arc<McpServerManager>,
    server: String,
    tool: String,
    definition: ConfiguredToolDefinition,
    marker: PhantomData<fn() -> (Security, Selection)>,
}

impl<Security: AccessSnapshot + 'static, Selection: Send + 'static> Tool
    for McpTool<Security, Selection>
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = catalog::Invocation;
    type Selection = Selection;
    type Security = Security;
    type Output = catalog::Output;
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
        invocation: catalog::Invocation,
        scope: InvocationScope<'a, Security, Selection>,
    ) -> BoxFuture<'a, Result<catalog::Output, OperationError<McpToolError>>> {
        Box::pin(async move {
            let outcome = self
                .manager
                .call_tool(&self.server, &self.tool, &invocation.arguments_json)
                .await;
            let text = match outcome {
                Ok(outcome) if !outcome.is_error => outcome.text,
                Ok(outcome) => invocation
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
                        OperationError::Failed(error) => OperationError::Failed(McpToolError::new(
                            &self.server,
                            McpToolFailureKind::Transport,
                            error.to_string(),
                        )),
                    })?,
                Err(error) => return Err(OperationError::Failed(error)),
            };
            Ok(text.into())
        })
    }
}

/// One factory per live MCP tool, named `mcp__<server>__<tool>`. Definitions
/// come from the manager's `tools/list` snapshot, not from the config catalog.
pub struct McpFactories<S, T> {
    factories: Vec<catalog::Factory<S, T>>,
}

impl<S, T> McpFactories<S, T> {
    pub fn into_factories(self) -> Vec<catalog::Factory<S, T>> {
        self.factories
    }
}

impl<S: AccessSnapshot + Clone + 'static, T: Send + Sync + Clone + 'static> McpFactories<S, T> {
    pub fn new(
        manager: Arc<McpServerManager>,
        tools: &BTreeMap<String, BTreeMap<String, ConfiguredToolDefinition>>,
    ) -> Self {
        let factories = tools
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
                    catalog::extension(
                        format!("mcp__{server}__{tool_name}"),
                        &[],
                        true,
                        tool,
                        catalog::scoped,
                    )
                })
            })
            .collect();
        Self { factories }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::testing::stdio_server;
    use parrot_domain::contracts::async_ops::Cancellation;
    use parrot_domain::contracts::paths::ResolvedRead;
    use parrot_foundation::template_catalog::PromptTemplateCatalog;
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

    async fn fixture_manager() -> Arc<McpServerManager> {
        McpServerManager::connect(&BTreeMap::from([(
            "fixture".to_owned(),
            stdio_server("sh", true),
        )]))
        .await
    }

    fn selection() -> catalog::Selection<()> {
        catalog::Selection {
            requested_model: String::new(),
            scoped: (),
        }
    }

    #[tokio::test]
    async fn factories_carry_namespaced_names_and_server_schemas() {
        let manager = fixture_manager().await;
        let tools = manager.connected_tools();
        let factory =
            &McpFactories::<TestSecurity, ()>::new(manager.clone(), &tools).into_factories()[0];
        assert_eq!(factory.name(), "mcp__fixture__echo");
        assert_eq!(factory.aliases(), &[] as &[&str]);
        let definition = factory.definition();
        assert_eq!(definition.description, "Echo tool");
        assert_eq!(definition.parameters_json, "{\"type\":\"object\"}");
        assert_eq!(
            factory
                .create()
                .parallel_safety(&catalog::Invocation::new("call", "{}", templates())),
            ParallelSafety::Sequential
        );
        manager.close().await;
    }

    #[tokio::test]
    async fn invocation_forwards_arguments_verbatim_and_returns_text() {
        let manager = fixture_manager().await;
        let tools = manager.connected_tools();
        let factory =
            &McpFactories::<TestSecurity, ()>::new(manager.clone(), &tools).into_factories()[0];
        let outcome = factory
            .create()
            .invoke(
                catalog::Invocation::new("call-1", r#"{"text":"hi"}"#, templates()),
                InvocationScope::new(TestSecurity, selection(), &NoCancel),
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, r#"echo: {"text":"hi"}"#);
        manager.close().await;
    }

    #[tokio::test]
    async fn server_error_results_render_through_the_error_template() {
        let manager = fixture_manager().await;
        let tools = manager.connected_tools();
        let factory =
            &McpFactories::<TestSecurity, ()>::new(manager.clone(), &tools).into_factories()[0];
        let outcome = factory
            .create()
            .invoke(
                catalog::Invocation::new("call-2", r#"{"text":"boom"}"#, templates()),
                InvocationScope::new(TestSecurity, selection(), &NoCancel),
            )
            .await
            .unwrap();
        assert!(outcome.text.contains("boom"));
        manager.close().await;
    }
}
