//! Source-compatible file tools over captured, workspace-bound I/O services.

pub mod diff;
mod glob;
mod image;
mod input;
mod mutation;
pub mod outline;
mod read;
mod workspace;

use std::{fmt, sync::Arc};

use parrot_domain::{
    contracts::async_ops::{Cancellation, OperationError, OperationResult},
    values::tools::ToolCycleImageBudget,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::artifacts::ImageArtifactMetadata;
use serde_json::Value;

pub use glob::GlobTool;
pub use image::ReadImageTool;
pub use mutation::{EditTool, WriteTool};
pub use read::ReadTool;
pub use workspace::WorkspacePaths;

pub struct FileToolInvocation {
    pub call_id: String,
    pub arguments_json: String,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
    pub image_budget: Option<Arc<ImageBudget>>,
}

/// One existing domain budget paired with its source-owned failure renderer.
pub struct ImageBudget {
    pub budget: Arc<ToolCycleImageBudget>,
    templates: Arc<PromptTemplateCatalog>,
}

impl ImageBudget {
    pub fn new(budget: Arc<ToolCycleImageBudget>, templates: Arc<PromptTemplateCatalog>) -> Self {
        Self { budget, templates }
    }

    fn describe_failure(&self) -> OperationResult<String, FileToolError> {
        self.budget.describe_failure(|id, arguments| {
            let arguments = arguments
                .iter()
                .map(|(key, value)| ((*key).into(), Value::String(value.clone())))
                .collect::<Vec<_>>();
            self.templates
                .render(id, &arguments, &SynchronousFormatting)
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => {
                        OperationError::Failed(FileToolError::Template(error.to_string()))
                    }
                })
        })
    }
}

impl FileToolInvocation {
    pub fn new(call_id: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            arguments_json: arguments_json.into(),
            prompt_templates: None,
            image_budget: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileToolOutcome {
    #[default]
    Finished,
    ImageBudgetExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileToolResult {
    pub text: String,
    pub image_artifacts: Vec<ImageArtifactMetadata>,
    pub outcome: FileToolOutcome,
}

impl FileToolResult {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image_artifacts: Vec::new(),
            outcome: FileToolOutcome::Finished,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileToolError {
    File(parrot_domain::contracts::filesystem::FileReadError),
    Template(String),
    Artifact(String),
    GlobTimeout,
}

impl fmt::Display for FileToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(error) => error.fmt(formatter),
            Self::Template(message) | Self::Artifact(message) => formatter.write_str(message),
            Self::GlobTimeout => formatter.write_str("glob timed out"),
        }
    }
}

impl std::error::Error for FileToolError {}

// Source IPromptTemplateCatalog.Render uses CancellationToken.None; tool I/O
// still observes invocation cancellation independently of synchronous formatting.
struct SynchronousFormatting;
impl Cancellation for SynchronousFormatting {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

struct Formatter<'a> {
    invocation: &'a FileToolInvocation,
}

impl<'a> Formatter<'a> {
    fn new(invocation: &'a FileToolInvocation) -> Self {
        Self { invocation }
    }

    fn render(
        &self,
        id: &str,
        key: &str,
        value: &str,
        fallback: String,
    ) -> OperationResult<String, FileToolError> {
        match &self.invocation.prompt_templates {
            Some(templates) => templates
                .render(
                    id,
                    &[(key.into(), Value::String(value.into()))],
                    &SynchronousFormatting,
                )
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => {
                        OperationError::Failed(FileToolError::Template(error.to_string()))
                    }
                }),
            None => Ok(fallback),
        }
    }

    fn text(&self, text: &str) -> OperationResult<String, FileToolError> {
        self.render("tool-result.text", "value", text, text.into())
    }

    fn error(&self, message: &str) -> OperationResult<FileToolResult, FileToolError> {
        self.render(
            "tool-result.error",
            "message",
            message,
            format!("error: {message}"),
        )
        .map(FileToolResult::text)
    }

    fn outline(&self, outline: &str) -> OperationResult<String, FileToolError> {
        self.render(
            "tool-result.markdown-outline",
            "outline",
            outline,
            format!("markdown outline (line: heading):\n{outline}\n"),
        )
    }
}

fn file_error(
    error: OperationError<parrot_domain::contracts::filesystem::FileReadError>,
) -> OperationError<FileToolError> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => OperationError::Failed(FileToolError::File(error)),
    }
}
