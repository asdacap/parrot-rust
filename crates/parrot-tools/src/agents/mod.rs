//! Retained agent and interaction tools over caller-scoped domain services.

pub mod input;
pub mod questions;
pub mod status;

use std::fmt;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::runtime::AgentRuntime;
use parrot_domain::contracts::security::InvocationContext;
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::ConfiguredToolDefinition;

use input::{SendInput, SpawnInput, WaitInput, parse_interrupt_name, parse_status_name};
use status::{StatusFormatter, StatusSnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentToolError {
    Registry(String),
    Provider(String),
    Argument(String),
    Question(String),
    QuestionRejected(String),
    Service(String),
}
impl fmt::Display for AgentToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Registry(message)
            | Self::Provider(message)
            | Self::Argument(message)
            | Self::Question(message)
            | Self::QuestionRejected(message)
            | Self::Service(message) => message,
        })
    }
}
impl std::error::Error for AgentToolError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInvocation {
    pub arguments_json: String,
    pub assistant_sequence: i64,
    pub call_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSelection {
    pub requested_model: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnRequest {
    pub input: SpawnInput,
    pub assistant_sequence: i64,
    pub call_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnResult {
    pub name: String,
    pub depth: i32,
    pub output_path: String,
}
impl SpawnResult {
    pub fn format(&self) -> String {
        format!(
            "{{\"name\":{},\"kind\":\"agent\",\"status\":\"running\",\"depth\":{},\"output\":{}}}",
            json_string(&self.name),
            self.depth,
            json_string(&self.output_path)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendRequest {
    pub input: SendInput,
    /// The caller-bound runtime must enforce this after resolving the recipient,
    /// including every ancestor alias, not just a literal `parent` spelling.
    pub allow_parent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendResult {
    pub name: String,
    pub message_id: String,
    pub follow_up: bool,
}
impl SendResult {
    pub fn format(&self) -> String {
        if self.follow_up {
            format!(
                "{{\"name\":{},\"status\":\"running\"}}",
                json_string(&self.name)
            )
        } else {
            format!(
                "{{\"name\":{},\"message_id\":{}}}",
                json_string(&self.name),
                json_string(&self.message_id)
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedRequest {
    pub name: String,
}

/// Returns the actual router's requested selector, retaining alias identity rather
/// than substituting its canonical model. An omitted model inherits the captured
/// turn selector without resolving the parent's later selection.
pub trait ModelSelector: Send + Sync {
    fn resolve(&self, selector: &str) -> Result<String, AgentToolError>;
}

impl ModelSelector for parrot_provider::routing::aliases::ModelRouter {
    fn resolve(&self, selector: &str) -> Result<String, AgentToolError> {
        parrot_provider::routing::aliases::ModelRouter::resolve(self, selector)
            .map(|selection| selection.requested_selector.value)
            .map_err(|error| AgentToolError::Provider(error.to_string()))
    }
}

/// Bound prompt templates supplied by the selected session; no global catalog.
pub trait ResultTemplates: Send + Sync {
    fn render(&self, name: &str, arguments: &[(&str, &str)]) -> Result<String, AgentToolError>;
}

impl ResultTemplates for parrot_foundation::template_catalog::PromptTemplateCatalog {
    fn render(&self, name: &str, arguments: &[(&str, &str)]) -> Result<String, AgentToolError> {
        let arguments: Vec<_> = arguments
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_owned(),
                    serde_json::Value::String((*value).to_owned()),
                )
            })
            .collect();
        parrot_foundation::template_catalog::PromptTemplateCatalog::render(
            self,
            name,
            &arguments,
            &Uncancelled,
        )
        .map_err(|error| AgentToolError::Service(error.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentToolKind {
    Spawn,
    Send,
    Interrupt,
    Status,
    Wait,
}
impl AgentToolKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "agent_spawn",
            Self::Send => "agent_send",
            Self::Interrupt => "agent_interrupt",
            Self::Status => "agent_status",
            Self::Wait => "wait",
        }
    }
}

pub struct AgentTool<R, M> {
    kind: AgentToolKind,
    definition: ConfiguredToolDefinition,
    runtime: Arc<R>,
    models: Arc<M>,
    templates: Option<Arc<dyn ResultTemplates>>,
    allow_parent: bool,
    status: StatusFormatter,
}
impl<R, M> AgentTool<R, M> {
    pub fn new(
        kind: AgentToolKind,
        definition: ConfiguredToolDefinition,
        runtime: Arc<R>,
        models: Arc<M>,
        templates: Option<Arc<dyn ResultTemplates>>,
        allow_parent: bool,
    ) -> Self {
        Self {
            kind,
            definition,
            runtime,
            models,
            templates,
            allow_parent,
            status: StatusFormatter::default(),
        }
    }
    pub fn name(&self) -> &'static str {
        self.kind.name()
    }
    fn text(&self, text: &str) -> Result<String, AgentToolError> {
        render(
            self.templates.as_deref(),
            "tool-result.text",
            &[("value", text)],
            || text.to_owned(),
        )
    }
    fn error(&self, message: &str) -> Result<String, AgentToolError> {
        render(
            self.templates.as_deref(),
            "tool-result.error",
            &[("message", message)],
            || format!("error: {message}"),
        )
    }
}

impl<R, M> Tool for AgentTool<R, M>
where
    R: AgentRuntime<
            SpawnRequest = SpawnRequest,
            SpawnResult = SpawnResult,
            SendRequest = SendRequest,
            SendResult = SendResult,
            InterruptRequest = NamedRequest,
            InterruptResult = (),
            StatusRequest = NamedRequest,
            StatusResult = StatusSnapshot,
            WaitRequest = WaitInput,
            WaitResult = String,
            Error = AgentToolError,
        >,
    R::Security: Clone,
    M: ModelSelector,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ToolInvocation;
    type Selection = AgentSelection;
    type Security = R::Security;
    type Output = String;
    type Error = AgentToolError;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        self.kind != AgentToolKind::Spawn
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, Result<Self::Output, OperationError<Self::Error>>> {
        Box::pin(async move {
            let operation = match self.kind {
                AgentToolKind::Spawn => {
                    let mut input = match SpawnInput::parse(&invocation.arguments_json) {
                        Ok(input) => input,
                        Err(error) => return self.error(&error.0).map_err(OperationError::Failed),
                    };
                    input.model = if input.model.is_empty() {
                        context.selection().requested_model.clone()
                    } else {
                        match self.models.resolve(&input.model) {
                            Ok(model) => model,
                            Err(
                                error @ (AgentToolError::Registry(_)
                                | AgentToolError::Provider(_)
                                | AgentToolError::Argument(_)),
                            ) => {
                                return self
                                    .error(&error.to_string())
                                    .map_err(OperationError::Failed);
                            }
                            Err(error) => return Err(OperationError::Failed(error)),
                        }
                    };
                    self.runtime
                        .spawn(
                            SpawnRequest {
                                input,
                                assistant_sequence: invocation.assistant_sequence,
                                call_id: invocation.call_id,
                            },
                            context.security().clone(),
                            context.cancellation(),
                        )
                        .await
                        .map(|result| result.format())
                }
                AgentToolKind::Send => {
                    let input = match SendInput::parse(&invocation.arguments_json) {
                        Ok(input) => input,
                        Err(error) => return self.error(&error.0).map_err(OperationError::Failed),
                    };
                    self.runtime
                        .send(
                            SendRequest {
                                input,
                                allow_parent: self.allow_parent,
                            },
                            context.cancellation(),
                        )
                        .await
                        .map(|result| result.format())
                }
                AgentToolKind::Interrupt => {
                    let name = match parse_interrupt_name(&invocation.arguments_json) {
                        Ok(name) => name,
                        Err(error) => return self.error(&error.0).map_err(OperationError::Failed),
                    };
                    self.runtime.interrupt(NamedRequest { name: name.clone() }, &Uncancelled).await
                        .map(|()| format!("Interrupt requested for child agent '{name}'. Its stop is reported through its completion event."))
                        .and_then(|message| self.text(&message).map_err(OperationError::Failed))
                }
                AgentToolKind::Status => {
                    let name = match parse_status_name(&invocation.arguments_json) {
                        Ok(name) => name,
                        Err(error) => return self.error(&error.0).map_err(OperationError::Failed),
                    };
                    self.runtime
                        .status(NamedRequest { name })
                        .map(|snapshot| self.status.format(&snapshot))
                        .map_err(OperationError::Failed)
                }
                AgentToolKind::Wait => {
                    let input = match WaitInput::parse(&invocation.arguments_json) {
                        Ok(input) => input,
                        Err(error) => return self.error(&error.0).map_err(OperationError::Failed),
                    };
                    self.runtime.wait(input, context.cancellation()).await
                }
            };
            match operation {
                Err(OperationError::Failed(error))
                    if matches!(error, AgentToolError::Registry(_))
                        || self.kind == AgentToolKind::Spawn
                            && matches!(
                                error,
                                AgentToolError::Provider(_) | AgentToolError::Argument(_)
                            ) =>
                {
                    self.error(&error.to_string())
                        .map_err(OperationError::Failed)
                }
                other => other,
            }
        })
    }
}

pub fn send_definition(
    configured: ConfiguredToolDefinition,
    allow_parent: bool,
    templates: &dyn ResultTemplates,
) -> Result<ConfiguredToolDefinition, AgentToolError> {
    if allow_parent {
        return Ok(configured);
    }
    Ok(ConfiguredToolDefinition {
        description: templates.render("agent-send-tool.description-without-parent", &[])?,
        parameters_json: configured.parameters_json,
    })
}

pub(super) fn render(
    templates: Option<&dyn ResultTemplates>,
    key: &str,
    arguments: &[(&str, &str)],
    fallback: impl FnOnce() -> String,
) -> Result<String, AgentToolError> {
    templates.map_or_else(
        || Ok(fallback()),
        |templates| templates.render(key, arguments),
    )
}

struct Uncancelled;
impl Cancellation for Uncancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn json_string(value: &str) -> String {
    use std::fmt::Write;
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' if !matches!(character, '"' | '\'' | '&' | '<' | '>' | '+' | '`') => {
                output.push(character)
            }
            _ => {
                let mut units = [0; 2];
                for unit in character.encode_utf16(&mut units) {
                    write!(output, "\\u{unit:04X}").expect("string write");
                }
            }
        }
    }
    output.push('"');
    output
}
