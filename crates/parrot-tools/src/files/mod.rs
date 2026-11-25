//! Source-compatible file tools over captured, workspace-bound I/O services.

pub mod diff;
mod glob;
mod image;
mod input;
mod mutation;
pub mod outline;
mod read;
mod workspace;

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    sync::{Arc, Mutex},
};

use parrot_core::NeverCancelled;
use parrot_domain::{
    contracts::async_ops::{OperationError, OperationResult},
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
    pub prompt_templates: Arc<PromptTemplateCatalog>,
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
                .render(id, &arguments, &NeverCancelled)
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
    pub fn new(
        call_id: impl Into<String>,
        arguments_json: impl Into<String>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            arguments_json: arguments_json.into(),
            prompt_templates: templates,
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
    pub steers: Vec<String>,
    pub outcome: FileToolOutcome,
    pub display_result: Option<String>,
}

impl FileToolResult {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image_artifacts: Vec::new(),
            steers: Vec::new(),
            outcome: FileToolOutcome::Finished,
            display_result: None,
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

struct Formatter<'a> {
    invocation: &'a FileToolInvocation,
}

impl<'a> Formatter<'a> {
    fn new(invocation: &'a FileToolInvocation) -> Self {
        Self { invocation }
    }

    fn render(&self, id: &str, key: &str, value: &str) -> OperationResult<String, FileToolError> {
        self.invocation
            .prompt_templates
            .render(
                id,
                &[(key.into(), Value::String(value.into()))],
                &NeverCancelled,
            )
            .map_err(|error| match error {
                OperationError::Cancelled => OperationError::Cancelled,
                OperationError::Failed(error) => {
                    OperationError::Failed(FileToolError::Template(error.to_string()))
                }
            })
    }

    fn text(&self, text: &str) -> OperationResult<String, FileToolError> {
        self.render("tool-result.text", "value", text)
    }

    fn error(&self, message: &str) -> OperationResult<FileToolResult, FileToolError> {
        self.render("tool-result.error", "message", message)
            .map(FileToolResult::text)
    }

    fn outline(&self, outline: &str) -> OperationResult<String, FileToolError> {
        self.render("tool-result.markdown-outline", "outline", outline)
    }

    fn note(&self, short: &str) -> OperationResult<String, FileToolError> {
        self.render("tool-result.path-variable-note", "short", short)
    }
}

/// The number of paths one agent session's tip ledger remembers before evicting
/// the least recently used one.
const PATH_TIP_CAPACITY: usize = 256;

/// Records the paths that already received a short-path tip within one agent
/// session. Tips are advisory, so the ledger stays in memory and never persists.
pub struct PathTipLedger {
    enabled: bool,
    state: Mutex<AdmittedPaths>,
}

struct AdmittedPaths {
    paths: HashSet<String>,
    order: VecDeque<String>,
}

impl PathTipLedger {
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            state: Mutex::new(AdmittedPaths {
                paths: HashSet::new(),
                order: VecDeque::new(),
            }),
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            state: Mutex::new(AdmittedPaths {
                paths: HashSet::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Returns true only for the first admission of a path while enabled.
    pub fn admit(&self, path: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let mut state = self.state.lock().expect("path tip ledger poisoned");
        if state.paths.contains(path) {
            state.order.retain(|seen| seen != path);
        } else {
            state.paths.insert(path.to_owned());
        }
        state.order.push_back(path.to_owned());
        while state.paths.len() > PATH_TIP_CAPACITY {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.paths.remove(&evicted);
        }
        true
    }
}

/// Steers the path-variable advisory when the canonical shorthand for the input
/// path exists and has not been tipped in this agent session before. A call that
/// already used the `$NAME` shorthand records the path without tipping.
fn short_path_tip(
    paths: &WorkspacePaths,
    ledger: &PathTipLedger,
    input_path: &str,
    formatter: &Formatter<'_>,
    mut result: FileToolResult,
) -> OperationResult<FileToolResult, FileToolError> {
    let canonical = paths.expand_path(input_path);
    let Some(short) = paths.shorten(&canonical) else {
        return Ok(result);
    };
    if !ledger.admit(&short) {
        return Ok(result);
    }
    if !input_path.contains('$') {
        result.steers.push(formatter.note(&short)?);
    }
    Ok(result)
}

fn file_error(
    error: OperationError<parrot_domain::contracts::filesystem::FileReadError>,
) -> OperationError<FileToolError> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => OperationError::Failed(FileToolError::File(error)),
    }
}
