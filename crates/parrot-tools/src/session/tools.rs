use super::{Compaction, Failure, FailureKind, SessionServices, input};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::{
        context::{ContextCadence, ContextSize, ContextSnapshot},
        tools::ConfiguredToolDefinition,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct Invocation {
    pub call_id: String,
    pub assistant_sequence: i64,
    pub arguments_json: String,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
}

/// This family returns text only: no yielded process or attached image artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub text: String,
}

impl From<String> for Output {
    fn from(text: String) -> Self {
        Self { text }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    SetCheckpoint,
    SetExitReminder,
    ClearExitReminder,
    Status,
    CompactContext,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::SetCheckpoint => "set_checkpoint",
            Self::SetExitReminder => "set_exit_reminder",
            Self::ClearExitReminder => "clear_exit_reminder",
            Self::Status => "status",
            Self::CompactContext => "compact_context",
        }
    }
}

pub struct SessionTool<S: SessionServices> {
    kind: Kind,
    definition: ConfiguredToolDefinition,
    services: Arc<S>,
    templates: Arc<PromptTemplateCatalog>,
}

impl<S: SessionServices> SessionTool<S> {
    pub fn new(
        kind: Kind,
        definition: ConfiguredToolDefinition,
        services: Arc<S>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Self {
        Self {
            kind,
            definition,
            services,
            templates,
        }
    }

    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    async fn execute(
        &self,
        invocation: &Invocation,
        selection: &S::Selection,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Output, Failure> {
        match self.kind {
            Kind::SetCheckpoint => {
                let result = async {
                    let object = input::object(&invocation.arguments_json, &["title"])?;
                    let title = input::required_string(&object, "title")?;
                    self.services
                        .checkpoint(title, invocation.assistant_sequence, &invocation.call_id)
                        .await?;
                    Ok(title.to_owned().into())
                }
                .await;
                self.catch(invocation, result.map_err(OperationError::Failed))
            }
            Kind::SetExitReminder | Kind::ClearExitReminder => {
                self.catch(invocation, self.reminder(invocation, cancellation).await)
            }
            Kind::Status => {
                if let Err(error) = input::object(&invocation.arguments_json, &[]) {
                    return error_output(invocation, &error.message);
                }
                let runtime = self
                    .services
                    .runtime_status(selection, cancellation)
                    .await?;
                let statistics = self
                    .services
                    .statistics_status(selection, cancellation)
                    .await?;
                let status = [runtime, statistics]
                    .into_iter()
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if status.trim().is_empty() {
                    text_output(invocation, "No runtime status is currently available.")
                } else {
                    Ok(status.into())
                }
            }
            Kind::CompactContext => {
                let target = (|| {
                    let object =
                        input::object(&invocation.arguments_json, &["target_context_size"])?;
                    input::optional_string(&object, "target_context_size")?
                        .map(|text| {
                            if text.is_empty() {
                                return Err(input::format(
                                    "target_context_size must not be empty.",
                                ));
                            }
                            ContextSize::parse(text)
                                .map_err(|error| input::format(error.to_string()))
                        })
                        .transpose()
                })();
                let target = match target {
                    Ok(target) => target,
                    Err(error) => return error_output(invocation, &error.message),
                };
                let Compaction {
                    reduced,
                    mut context,
                } = self
                    .services
                    .compact(selection, target, cancellation)
                    .await?;
                let mut result = self.render_compaction(reduced, &context)?;
                for _ in 0..8 {
                    let observed = self
                        .services
                        .estimate_after_tool_result(selection, &invocation.call_id, &result)
                        .map_err(OperationError::Failed)?;
                    let rendered = self.render_compaction(reduced, &observed)?;
                    context = observed;
                    if rendered == result {
                        return Ok(result.into());
                    }
                    result = rendered;
                }
                Ok(self.render_compaction(reduced, &context)?.into())
            }
        }
    }

    async fn reminder(
        &self,
        invocation: &Invocation,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Output, Failure> {
        let allowed = if self.kind == Kind::SetExitReminder {
            &["title", "description"][..]
        } else {
            &["title"][..]
        };
        // Unlike ToolInputConversion, these two original tools deserialize null
        // and then use their own nonempty-field diagnostics.
        let object = if invocation.arguments_json.trim() == "null" {
            serde_json::Map::new()
        } else {
            input::object(&invocation.arguments_json, allowed).map_err(OperationError::Failed)?
        };
        let title = input::optional_string(&object, "title")
            .map_err(OperationError::Failed)?
            .unwrap_or("");
        if self.kind == Kind::SetExitReminder {
            let description = input::optional_string(&object, "description")
                .map_err(OperationError::Failed)?
                .unwrap_or("");
            if title.is_empty() || description.is_empty() {
                return error_output(
                    invocation,
                    "set_exit_reminder requires a nonempty title and description.",
                );
            }
            self.services
                .set_reminder(title, description, cancellation)
                .await?;
            let text = render(
                &self.templates,
                "set-exit-reminder-tool.set",
                &[
                    ("title", title.to_owned()),
                    ("description", description.to_owned()),
                ],
            )?;
            text_output(invocation, &text)
        } else {
            if title.is_empty() {
                return error_output(invocation, "clear_exit_reminder requires a nonempty title.");
            }
            if self.services.clear_reminder(title, cancellation).await? {
                let text = render(
                    &self.templates,
                    "clear-exit-reminder-tool.cleared",
                    &[("title", title.to_owned())],
                )?;
                text_output(invocation, &text)
            } else {
                let text = render(
                    &self.templates,
                    "clear-exit-reminder-tool.not-found",
                    &[
                        ("title", title.to_owned()),
                        ("titles", self.services.reminder_titles().await.join(", ")),
                    ],
                )?;
                error_output(invocation, &text)
            }
        }
    }

    fn catch(
        &self,
        invocation: &Invocation,
        result: OperationResult<Output, Failure>,
    ) -> OperationResult<Output, Failure> {
        match result {
            Err(OperationError::Failed(error))
                if matches!(
                    error.kind,
                    FailureKind::Json
                        | FailureKind::Format
                        | FailureKind::Argument
                        | FailureKind::InputConflict
                ) =>
            {
                error_output(invocation, &error.message)
            }
            result => result,
        }
    }

    fn render_compaction(
        &self,
        reduced: bool,
        context: &ContextSnapshot,
    ) -> OperationResult<String, Failure> {
        let outcome = if !context.is_available() {
            "unavailable"
        } else if reduced {
            "compacted"
        } else {
            "no-op"
        };
        self.templates
            .render(
                "compact-context-tool.result",
                &[
                    ("outcome".into(), outcome.into()),
                    (
                        "has_context_limit_override".into(),
                        context.has_context_limit_override.into(),
                    ),
                    ("estimated_tokens".into(), context.estimated_tokens.into()),
                    ("context_limit".into(), context.context_limit.into()),
                    (
                        "notification_interval".into(),
                        ContextCadence::NOTIFICATION_INTERVAL.into(),
                    ),
                    ("trigger".into(), context.trigger_percent.into()),
                    (
                        "trigger_tokens".into(),
                        context
                            .trigger_tokens
                            .map_or_else(|| "unavailable".into(), |value| value.to_string())
                            .into(),
                    ),
                    (
                        "usage".into(),
                        context
                            .usage_percent
                            .map_or_else(|| "unavailable".into(), |value| value.to_string())
                            .into(),
                    ),
                ],
                &RenderCancellation,
            )
            .map_err(|error| match error {
                OperationError::Cancelled => OperationError::Cancelled,
                OperationError::Failed(error) => {
                    OperationError::Failed(Failure::new(FailureKind::Argument, error.to_string()))
                }
            })
    }
}

impl<S: SessionServices> Tool for SessionTool<S> {
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = S::Selection;
    type Security = S::Security;
    type Output = Output;
    type Error = Failure;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<Output, Failure>> {
        Box::pin(async move {
            self.execute(&invocation, context.selection(), context.cancellation())
                .await
        })
    }
}

struct RenderCancellation;
impl Cancellation for RenderCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn render(
    templates: &PromptTemplateCatalog,
    id: &str,
    arguments: &[(&str, String)],
) -> OperationResult<String, Failure> {
    let arguments = arguments
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String(value.clone())))
        .collect::<Vec<_>>();
    templates
        .render(id, &arguments, &RenderCancellation)
        .map_err(|error| match error {
            OperationError::Cancelled => OperationError::Cancelled,
            OperationError::Failed(error) => {
                OperationError::Failed(Failure::new(FailureKind::Argument, error.to_string()))
            }
        })
}

pub(super) fn text_output(invocation: &Invocation, text: &str) -> OperationResult<Output, Failure> {
    match invocation.prompt_templates.as_deref() {
        Some(templates) => {
            render(templates, "tool-result.text", &[("value", text.to_owned())]).map(Into::into)
        }
        None => Ok(text.to_owned().into()),
    }
}

pub(super) fn error_output(
    invocation: &Invocation,
    message: &str,
) -> OperationResult<Output, Failure> {
    match invocation.prompt_templates.as_deref() {
        Some(templates) => render(
            templates,
            "tool-result.error",
            &[("message", message.to_owned())],
        )
        .map(Into::into),
        None => Ok(format!("error: {message}").into()),
    }
}
