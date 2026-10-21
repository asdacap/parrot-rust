//! Runtime-owned associated types for retained domain tools.
use crate::agent_turn::services::{TurnError, TurnResources, TurnSelection};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError},
    paths::ResolvedRead,
    security::AccessSnapshot,
    tools::Tool,
};
use parrot_domain::{
    protocol::messages::parrot::YieldedShellProcess, values::tools::ToolCycleImageBudget,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::{artifacts::ImageArtifactMetadata, diagnostics::FailureKind};
use std::{fmt, path::Path, sync::Arc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolMetadata {
    pub name: String,
    pub aliases: Vec<String>,
}

/// Clones the immutable capture handle, not its policy. The underlying snapshot
/// must obey the domain's immutability contract.
pub struct SharedSecurity<S: AccessSnapshot>(pub Arc<S>);
impl<S: AccessSnapshot> SharedSecurity<S> {
    pub fn snapshot(&self) -> &S {
        &self.0
    }
}
impl<S: AccessSnapshot> Clone for SharedSecurity<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<S: AccessSnapshot> AccessSnapshot for SharedSecurity<S> {
    type Error = S::Error;
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Self::Error> {
        self.0.check_read(read)
    }
    fn check_write(&self, target: &Path) -> Result<(), Self::Error> {
        self.0.check_write(target)
    }
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub call_id: String,
    pub arguments_json: String,
    pub assistant_sequence: i64,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
    pub image_budget: Option<Arc<ToolCycleImageBudget>>,
}
impl ToolInvocation {
    pub fn new(
        call_id: impl Into<String>,
        arguments_json: impl Into<String>,
        assistant_sequence: i64,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            arguments_json: arguments_json.into(),
            assistant_sequence,
            prompt_templates: None,
            image_budget: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    Completed,
    ImageBudgetExceeded,
}
#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub text: String,
    pub image_artifacts: Vec<ImageArtifactMetadata>,
    pub yielded_process: Option<YieldedShellProcess>,
    pub outcome: ToolOutcome,
}
impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image_artifacts: Vec::new(),
            yielded_process: None,
            outcome: ToolOutcome::Completed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolFailure {
    pub message: String,
    pub kind: FailureKind,
}
impl ToolFailure {
    pub fn new(message: impl Into<String>, kind: FailureKind) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
}
impl fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ToolFailure {}

pub type RuntimeTool<S> = dyn Tool<
        Definition = ToolMetadata,
        Invocation = ToolInvocation,
        Selection = TurnSelection<S>,
        Security = SharedSecurity<S>,
        Output = ToolOutput,
        Error = ToolFailure,
    >;

/// This in-drain boundary lends the actual already-owned state. It is not the
/// production retained Tool / SessionControl owned+'static bridge (P07/P08).
/// Implementations must not relock or schedule-and-wait on the current engine.
pub struct BorrowedToolContext<'a, S: AccessSnapshot> {
    pub selection: &'a TurnSelection<S>,
    pub resources: TurnResources<'a>,
    pub definitions: &'a [parrot_provider::sessions::Tool],
}
pub trait BorrowedTool<S: AccessSnapshot>: Send + Sync {
    fn definition(&self) -> &ToolMetadata;
    fn enabled_after_interruption(&self) -> bool;
    fn invoke<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: BorrowedToolContext<'a, S>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ToolOutput, OperationError<ToolFailure>>>;
}

pub enum ToolHandle<S: AccessSnapshot + 'static> {
    Ordinary(Arc<RuntimeTool<S>>),
    Borrowed(Arc<dyn BorrowedTool<S>>),
}
impl<S: AccessSnapshot + 'static> Clone for ToolHandle<S> {
    fn clone(&self) -> Self {
        match self {
            Self::Ordinary(tool) => Self::Ordinary(tool.clone()),
            Self::Borrowed(tool) => Self::Borrowed(tool.clone()),
        }
    }
}
impl<S: AccessSnapshot + 'static> ToolHandle<S> {
    pub fn definition(&self) -> &ToolMetadata {
        match self {
            Self::Ordinary(tool) => tool.definition(),
            Self::Borrowed(tool) => tool.definition(),
        }
    }
    pub fn enabled_after_interruption(&self) -> bool {
        match self {
            Self::Ordinary(tool) => tool.enabled_after_interruption(),
            Self::Borrowed(tool) => tool.enabled_after_interruption(),
        }
    }
}
pub struct ToolInventory<S: AccessSnapshot + 'static> {
    pub tools: Vec<ToolHandle<S>>,
    pub supported: Vec<bool>,
    pub definitions: Vec<parrot_domain::values::tools::ConfiguredToolDefinition>,
}
pub trait ToolFactory<S: AccessSnapshot + 'static>: Send + Sync {
    fn materialize(&self) -> Result<ToolInventory<S>, TurnError>;
}
