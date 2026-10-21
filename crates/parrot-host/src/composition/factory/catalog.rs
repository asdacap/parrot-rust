//! Retains the actual configured tool catalog at the runtime settlement boundary.
use crate::composition::context::{ContextSelection, RetainedCompactTool};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, OperationError},
    filesystem::FileReadErrorKind,
    security::{AccessSnapshot, InvocationContext},
    tools::{ParallelSafety, Tool},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::{
    agent_turn::services::{TurnError, TurnSelection},
    compaction::CompactionPlanner,
    tool_settlement::{
        SharedSecurity, ToolFactory, ToolFailure, ToolHandle, ToolInventory, ToolInvocation,
        ToolMetadata, ToolOutcome, ToolOutput,
    },
};
use parrot_storage::diagnostics::FailureKind;
use parrot_tools::{catalog, files};
use std::sync::Arc;

type ScopedCatalog<S> = catalog::Catalog<SharedSecurity<S>, ContextSelection<S>>;
type RetainedTool<S> = catalog::RetainedTool<SharedSecurity<S>, ContextSelection<S>>;

/// One adapter per agent scope. Neither materialization nor invocation rebuilds
/// a family tool: all handles below retain the catalog's original tool Arcs.
/// Context-reading status and context-mutating compaction both borrow the active
/// drain, rather than re-locking the backend that is awaiting their result.
pub struct CatalogAdapter<S: AccessSnapshot + 'static> {
    catalog: ScopedCatalog<S>,
    tools: Vec<ToolHandle<S>>,
}
impl<S: AccessSnapshot + 'static> CatalogAdapter<S> {
    pub fn new(
        catalog: ScopedCatalog<S>,
        planner: Arc<CompactionPlanner>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, TurnError> {
        let tools = catalog
            .factories()
            .iter()
            .map(|factory| {
                if matches!(factory.name(), "compact_context" | "status") {
                    let tool = if factory.name() == "status" {
                        RetainedCompactTool::status(factory, planner.clone(), templates.clone())
                    } else {
                        RetainedCompactTool::new(factory, planner.clone(), templates.clone())
                    };
                    tool.map(|tool| ToolHandle::Borrowed(Arc::new(tool)))
                        .map_err(|error| TurnError::Service(error.to_string()))
                } else {
                    Ok(ToolHandle::Ordinary(Arc::new(CatalogTool {
                        metadata: ToolMetadata {
                            name: factory.name().into(),
                            aliases: factory
                                .aliases()
                                .iter()
                                .map(|name| (*name).into())
                                .collect(),
                        },
                        tool: factory.create(),
                        templates: templates.clone(),
                    })))
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { catalog, tools })
    }

    pub fn catalog(&self) -> &ScopedCatalog<S> {
        &self.catalog
    }
}
impl<S: AccessSnapshot + 'static> ToolFactory<S> for CatalogAdapter<S> {
    fn materialize(&self) -> Result<ToolInventory<S>, TurnError> {
        Ok(ToolInventory {
            tools: self.tools.clone(),
            supported: self
                .catalog
                .factories()
                .iter()
                .map(|factory| factory.supports())
                .collect(),
            definitions: self
                .catalog
                .factories()
                .iter()
                .map(|factory| factory.definition().clone())
                .collect(),
        })
    }
}

struct CatalogTool<S: AccessSnapshot + 'static> {
    metadata: ToolMetadata,
    tool: Arc<RetainedTool<S>>,
    templates: Arc<PromptTemplateCatalog>,
}
impl<S: AccessSnapshot + 'static> Tool for CatalogTool<S> {
    type Definition = ToolMetadata;
    type Invocation = ToolInvocation;
    type Selection = TurnSelection<S>;
    type Security = SharedSecurity<S>;
    type Output = ToolOutput;
    type Error = ToolFailure;

    fn definition(&self) -> &ToolMetadata {
        &self.metadata
    }
    fn parallel_safety(&self, invocation: &ToolInvocation) -> ParallelSafety {
        self.tool
            .parallel_safety(&invocation_to_catalog(invocation.clone(), &self.templates))
    }
    fn enabled_after_interruption(&self) -> bool {
        self.tool.enabled_after_interruption()
    }
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: InvocationContext<'a, SharedSecurity<S>, TurnSelection<S>>,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>> {
        let selection = catalog::Selection {
            requested_model: context
                .selection()
                .model
                .requested_selector
                .value
                .as_str()
                .into(),
            scoped: ContextSelection::external(context.selection().clone()),
        };
        let retained_context = InvocationContext::new(
            context.security().clone(),
            selection,
            context.cancellation(),
        );
        let invocation = invocation_to_catalog(invocation, &self.templates);
        Box::pin(async move {
            self.tool
                .invoke(invocation, retained_context)
                .await
                .map(output_to_runtime)
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => {
                        OperationError::Failed(error_to_runtime(error))
                    }
                })
        })
    }
}

fn invocation_to_catalog(
    invocation: ToolInvocation,
    templates: &Arc<PromptTemplateCatalog>,
) -> catalog::Invocation {
    let image_templates = invocation
        .prompt_templates
        .as_ref()
        .unwrap_or(templates)
        .clone();
    catalog::Invocation {
        call_id: invocation.call_id,
        assistant_sequence: invocation.assistant_sequence,
        arguments_json: invocation.arguments_json,
        prompt_templates: invocation.prompt_templates,
        image_budget: invocation
            .image_budget
            .map(|budget| Arc::new(files::ImageBudget::new(budget, image_templates))),
    }
}
fn output_to_runtime(output: catalog::Output) -> ToolOutput {
    ToolOutput {
        text: output.text,
        image_artifacts: output.image_artifacts,
        yielded_process: output.yielded_process.map(|process| {
            use parrot_domain::protocol::messages::parrot::{
                ShellProcessActivityKind, YieldedShellProcess,
            };
            use parrot_tools::process::ownership::inventory::ActivityKind;
            YieldedShellProcess {
                process_id: process.process_id,
                name: process.name,
                inventory_instance_id: process.inventory_instance_id,
                visible_revision: process.visible_revision,
                stdout_path: process
                    .stdout_path
                    .map(|path| path.to_string_lossy().into_owned()),
                stderr_path: process
                    .stderr_path
                    .map(|path| path.to_string_lossy().into_owned()),
                activity_kind: match process.activity_kind {
                    ActivityKind::Execution => {
                        ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_EXECUTION
                    }
                    ActivityKind::Monitor => {
                        ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
                    }
                }
                .into(),
                ..YieldedShellProcess::new()
            }
        }),
        outcome: match output.outcome {
            files::FileToolOutcome::Finished => ToolOutcome::Completed,
            files::FileToolOutcome::ImageBudgetExceeded => ToolOutcome::ImageBudgetExceeded,
        },
    }
}
fn io_kind(error: &std::io::Error) -> FailureKind {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => FailureKind::AccessDenied,
        std::io::ErrorKind::TimedOut => FailureKind::Timeout,
        std::io::ErrorKind::InvalidInput => FailureKind::Argument,
        _ => FailureKind::Io,
    }
}
fn error_to_runtime(error: catalog::Error) -> ToolFailure {
    use parrot_tools::{agents::AgentToolError, session, tasks_queues};
    let kind = match &error {
        catalog::Error::File(files::FileToolError::File(error)) => match error.kind {
            FileReadErrorKind::Denied => FailureKind::AccessDenied,
            FileReadErrorKind::InvalidPath => FailureKind::Argument,
            FileReadErrorKind::InvalidOperation => FailureKind::InvalidOperation,
            FileReadErrorKind::NotSupported => FailureKind::Unexpected,
            _ => FailureKind::Io,
        },
        catalog::Error::File(files::FileToolError::GlobTimeout) => FailureKind::Timeout,
        catalog::Error::File(files::FileToolError::Template(_)) | catalog::Error::Definition(_) => {
            FailureKind::Argument
        }
        catalog::Error::File(files::FileToolError::Artifact(_)) => FailureKind::Io,
        catalog::Error::Process(error)
        | catalog::Error::WebFetch(parrot_tools::web_fetch::FetchError::Io(error)) => {
            io_kind(error)
        }
        catalog::Error::Agent(error) => match error {
            AgentToolError::Argument(_) => FailureKind::Argument,
            AgentToolError::Registry(_) | AgentToolError::QuestionRejected(_) => {
                FailureKind::InvalidOperation
            }
            _ => FailureKind::Unexpected,
        },
        catalog::Error::Session(error) => match error.kind {
            session::FailureKind::Json
            | session::FailureKind::Format
            | session::FailureKind::Argument => FailureKind::Argument,
            session::FailureKind::Unauthorized | session::FailureKind::Permission => {
                FailureKind::AccessDenied
            }
            session::FailureKind::Io => FailureKind::Io,
            session::FailureKind::InvalidOperation | session::FailureKind::InputConflict => {
                FailureKind::InvalidOperation
            }
            session::FailureKind::Network => FailureKind::Http,
            _ => FailureKind::Unexpected,
        },
        catalog::Error::TaskQueue(error) => match error.kind {
            tasks_queues::FailureKind::Json
            | tasks_queues::FailureKind::Format
            | tasks_queues::FailureKind::Argument => FailureKind::Argument,
            tasks_queues::FailureKind::Registry
            | tasks_queues::FailureKind::Queue
            | tasks_queues::FailureKind::InvalidOperation => FailureKind::InvalidOperation,
            tasks_queues::FailureKind::Io => FailureKind::Io,
            tasks_queues::FailureKind::Denied => FailureKind::AccessDenied,
            _ => FailureKind::Unexpected,
        },
        catalog::Error::WebFetch(_) => FailureKind::Argument,
    };
    ToolFailure::new(error.to_string(), kind)
}

#[cfg(test)]
#[path = "../../../tests/composition/factory/catalog.rs"]
mod tests;
