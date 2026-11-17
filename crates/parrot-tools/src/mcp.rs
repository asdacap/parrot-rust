//! MCP server tools exposed in the agent-session tool catalog.

use std::collections::BTreeMap;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::{
    McpToolCaller, McpToolError, McpToolFailureKind, ParallelSafety, Tool,
};
use parrot_domain::values::tools::ConfiguredToolDefinition;
use parrot_foundation::template_catalog::PromptTemplateCatalog;

use crate::catalog::{self, Factory};

pub struct Invocation {
    arguments_json: String,
    prompt_templates: Arc<PromptTemplateCatalog>,
}

impl From<catalog::Invocation> for Invocation {
    fn from(invocation: catalog::Invocation) -> Self {
        Self {
            arguments_json: invocation.arguments_json,
            prompt_templates: invocation.prompt_templates,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub text: String,
}

impl From<Output> for catalog::Output {
    fn from(output: Output) -> Self {
        output.text.into()
    }
}

pub struct McpTool<Security, Selection> {
    caller: Arc<dyn McpToolCaller>,
    server: String,
    tool: String,
    definition: ConfiguredToolDefinition,
    marker: std::marker::PhantomData<fn() -> (Security, Selection)>,
}

impl<Security: AccessSnapshot + 'static, Selection: Send + 'static> Tool
    for McpTool<Security, Selection>
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = Selection;
    type Security = Security;
    type Output = Output;
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
        invocation: Invocation,
        scope: InvocationScope<'a, Security, Selection>,
    ) -> BoxFuture<'a, Result<Output, OperationError<McpToolError>>> {
        Box::pin(async move {
            let outcome = match self
                .caller
                .call_mcp_tool(
                    self.server.clone(),
                    self.tool.clone(),
                    invocation.arguments_json,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    return Err(OperationError::Failed(error));
                }
            };
            let text = if outcome.is_error {
                invocation
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
                            let reason = error.to_string();
                            OperationError::Failed(McpToolError::new(
                                &self.server,
                                McpToolFailureKind::Transport,
                                reason,
                            ))
                        }
                    })?
            } else {
                outcome.text
            };
            Ok(Output { text })
        })
    }
}

/// One factory per live MCP tool, named `mcp__<server>__<tool>`. Definitions
/// come from the manager's `tools/list` snapshot, not from the config catalog.
pub struct McpFactories<S, T> {
    factories: Vec<Factory<S, T>>,
}

impl<S, T> McpFactories<S, T> {
    pub fn into_factories(self) -> Vec<Factory<S, T>> {
        self.factories
    }
}

impl<S: AccessSnapshot + Clone + 'static, T: Send + Sync + Clone + 'static> McpFactories<S, T> {
    pub fn new(
        caller: Arc<dyn McpToolCaller>,
        tools: &BTreeMap<String, BTreeMap<String, ConfiguredToolDefinition>>,
    ) -> Self {
        let factories = tools
            .iter()
            .flat_map(|(server, tools)| {
                let caller = caller.clone();
                tools.iter().map(move |(tool_name, definition)| {
                    let tool = McpTool {
                        caller: caller.clone(),
                        server: server.clone(),
                        tool: tool_name.clone(),
                        definition: definition.clone(),
                        marker: std::marker::PhantomData,
                    };
                    catalog::bind(
                        format!("mcp__{server}__{tool_name}"),
                        &[],
                        true,
                        tool,
                        crate::catalog::scoped,
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
    use crate::catalog::Invocation;
    use parrot_domain::contracts::async_ops::Cancellation;
    use parrot_domain::contracts::tools::{McpToolOutcome, ParallelSafety};
    use std::future::pending;
    use std::sync::Mutex;

    struct NoCancel;
    impl Cancellation for NoCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(pending())
        }
    }

    struct FakeCaller {
        outcome: McpToolOutcome,
        seen: Mutex<Vec<String>>,
    }
    impl McpToolCaller for FakeCaller {
        fn call_mcp_tool<'a>(
            &'a self,
            server: String,
            tool: String,
            arguments_json: String,
        ) -> BoxFuture<'a, Result<McpToolOutcome, McpToolError>> {
            self.seen
                .lock()
                .expect("fake caller poisoned")
                .push(format!("{server} {tool} {arguments_json}"));
            let outcome = self.outcome.clone();
            Box::pin(async move { Ok(outcome) })
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

    fn tools() -> BTreeMap<String, BTreeMap<String, ConfiguredToolDefinition>> {
        BTreeMap::from([(
            "fixture".to_owned(),
            BTreeMap::from([(
                "echo".to_owned(),
                ConfiguredToolDefinition::new("Echo tool", "{\"type\":\"object\"}"),
            )]),
        )])
    }

    fn caller(text: &str, is_error: bool) -> FakeCaller {
        FakeCaller {
            outcome: McpToolOutcome {
                text: text.to_owned(),
                is_error,
            },
            seen: Mutex::new(Vec::new()),
        }
    }

    #[derive(Clone)]
    struct TestSecurity;
    impl AccessSnapshot for TestSecurity {
        type Error = std::io::Error;
        fn check_read(
            &self,
            _read: &parrot_domain::contracts::paths::ResolvedRead,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn check_write(&self, _target: &std::path::Path) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn factories_carry_namespaced_names_and_server_schemas() {
        let factories =
            McpFactories::<TestSecurity, ()>::new(Arc::new(caller("ok", false)), &tools())
                .into_factories();
        assert_eq!(factories.len(), 1);
        let factory = &factories[0];
        assert_eq!(factory.name(), "mcp__fixture__echo");
        assert_eq!(factory.aliases(), &[] as &[&str]);
        let definition = factory.definition();
        assert_eq!(definition.description, "Echo tool");
        assert_eq!(definition.parameters_json, "{\"type\":\"object\"}");
        assert_eq!(
            factory
                .create()
                .parallel_safety(&Invocation::new("call", "{}", templates(),)),
            ParallelSafety::Sequential
        );
    }

    #[tokio::test]
    async fn invocation_forwards_arguments_verbatim_and_returns_text() {
        let fake = Arc::new(caller("hello from fake", false));
        let caller: Arc<dyn McpToolCaller> = fake.clone();
        let factories = McpFactories::<TestSecurity, ()>::new(caller, &tools()).into_factories();
        let tool = factories[0].create();
        let outcome = tool
            .invoke(
                Invocation::new("call-1", "{\"text\":\"hi\"}", templates()),
                InvocationScope::new(
                    TestSecurity,
                    crate::catalog::Selection {
                        requested_model: String::new(),
                        scoped: (),
                    },
                    &NoCancel,
                ),
            )
            .await
            .unwrap();
        assert_eq!(outcome.text, "hello from fake");
        let seen = fake.seen.lock().expect("fake caller poisoned").clone();
        assert_eq!(seen, vec!["fixture echo {\"text\":\"hi\"}".to_owned()]);
    }

    #[tokio::test]
    async fn server_error_results_render_through_the_error_template() {
        let factories =
            McpFactories::<TestSecurity, ()>::new(Arc::new(caller("boom", true)), &tools())
                .into_factories();
        let tool = factories[0].create();
        let outcome = tool
            .invoke(
                Invocation::new("call-2", "{}", templates()),
                InvocationScope::new(
                    TestSecurity,
                    crate::catalog::Selection {
                        requested_model: String::new(),
                        scoped: (),
                    },
                    &NoCancel,
                ),
            )
            .await
            .unwrap();
        assert!(outcome.text.contains("boom"));
    }
}
