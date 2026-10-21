//! Source-shaped process tools over the retained scoped process owner.
//!
//! Launch policy, physical workspace resolution, agent lookup and monitor
//! delivery remain supplied services. These wrappers do not grant authority or
//! replace the native execution/ownership implementations.

pub mod input;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::security::InvocationContext;
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::values::tools::{ConfiguredToolDefinition, ToolDefinitionCatalog};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::Value;

use super::execution::runner::TerminalMode;
use super::ownership::domain::CapturedProcessLauncher;
use super::ownership::execution::NeverCancelled;
use super::ownership::inventory::YieldedShellProcess;
use super::ownership::managed::{CompletionReport, ProcessReport};
use super::ownership::monitor::MonitorSink;
use super::ownership::monitor_report::MonitorProcessReport;
use super::ownership::owner::{ProcessAgent, ProcessStart, ShellProcessOwner};
use crate::sandbox::linux::ReadOnlyExecCommandClassifier;
use input::{ExecInput, InterruptInput, MonitorInput, WriteStdinInput};

pub const NAMED_COMMAND_LENGTH: usize = 300;
pub const LONG_COMMAND_LENGTH: usize = 200;

/// Overrides only; the platform launcher owns the inherited environment and
/// selected resources. Values are passed literally, never shell-expanded here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub command: String,
    pub environment: Vec<(String, String)>,
    pub terminal_mode: TerminalMode,
}

/// Read-only metadata used solely for the redundant `cd` diagnostic. This is
/// not a filesystem authorization or mutation service.
pub trait ProcessWorkspace: Send + Sync {
    fn root(&self) -> &str;
    fn resolves_to_root(&self, target: &str) -> bool;
}

pub struct ResolvedProcess {
    pub owner: Arc<ShellProcessOwner>,
    pub name: String,
}

/// Resolve only explicit slash-separated resource addresses. Bare process names
/// never consult this service or fall back to a different owner.
pub trait ProcessResolver: Send + Sync {
    fn resolve(&self, address: &str) -> io::Result<ResolvedProcess>;
}

pub struct ProcessToolServices<L: CapturedProcessLauncher<Request = CommandRequest>> {
    pub owner: Arc<ShellProcessOwner>,
    pub agent: ProcessAgent,
    pub launcher: Arc<L>,
    pub resolver: Arc<dyn ProcessResolver>,
    pub workspace: Arc<dyn ProcessWorkspace>,
    pub read_only_prefixes: Vec<String>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub monitor_sink: Arc<dyn MonitorSink>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessToolKind {
    ExecCommand,
    WriteStdin,
    InterruptProcess,
    Monitor,
}

impl ProcessToolKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::ExecCommand => "exec_command",
            Self::WriteStdin => "write_stdin",
            Self::InterruptProcess => "interrupt_process",
            Self::Monitor => "monitor",
        }
    }

    pub fn aliases(self) -> &'static [&'static str] {
        if self == Self::ExecCommand {
            &["bash"]
        } else {
            &[]
        }
    }
}

pub struct ProcessInvocation {
    pub call_id: String,
    pub arguments_json: String,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
}

impl ProcessInvocation {
    pub fn new(call_id: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            arguments_json: arguments_json.into(),
            prompt_templates: None,
        }
    }
}

pub struct ProcessToolResult {
    pub text: String,
    pub yielded_process: Option<YieldedShellProcess>,
}

impl ProcessToolResult {
    fn text(text: String) -> Self {
        Self {
            text,
            yielded_process: None,
        }
    }
}

pub struct ProcessToolFactory<L: CapturedProcessLauncher<Request = CommandRequest>> {
    kind: ProcessToolKind,
    definition: ConfiguredToolDefinition,
    services: Arc<ProcessToolServices<L>>,
}

impl<L: CapturedProcessLauncher<Request = CommandRequest>> ProcessToolFactory<L> {
    pub fn new(
        kind: ProcessToolKind,
        services: Arc<ProcessToolServices<L>>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, parrot_domain::values::tools::MissingToolDefinition> {
        Ok(Self {
            kind,
            definition: definitions.describe(kind.name())?.clone(),
            services,
        })
    }

    pub fn definition(&self) -> &ConfiguredToolDefinition {
        &self.definition
    }

    pub fn create(&self) -> ProcessTool<L> {
        ProcessTool {
            kind: self.kind,
            definition: self.definition.clone(),
            classifier: ReadOnlyExecCommandClassifier::new(
                self.services.read_only_prefixes.clone(),
            ),
            services: Arc::clone(&self.services),
        }
    }
}

pub struct ProcessTool<L: CapturedProcessLauncher<Request = CommandRequest>> {
    kind: ProcessToolKind,
    definition: ConfiguredToolDefinition,
    classifier: ReadOnlyExecCommandClassifier,
    services: Arc<ProcessToolServices<L>>,
}

impl<L: CapturedProcessLauncher<Request = CommandRequest>> ProcessTool<L>
where
    L::Security: Clone,
{
    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    pub fn aliases(&self) -> &'static [&'static str] {
        self.kind.aliases()
    }

    fn error(
        &self,
        invocation: &ProcessInvocation,
        message: &str,
    ) -> io::Result<ProcessToolResult> {
        let text = match &invocation.prompt_templates {
            Some(templates) => render(templates, "tool-result.error", &[("message", message)])?,
            None => format!("error: {message}"),
        };
        Ok(ProcessToolResult::text(text))
    }

    async fn start(
        &self,
        invocation: &ProcessInvocation,
        input: StartInput,
        security: &L::Security,
        report: Arc<dyn ProcessReport>,
    ) -> io::Result<super::ownership::managed::ClaimedProcess> {
        let process = ProcessStart {
            requested_name: input.name,
            command: input.command.clone(),
            description: input.description,
            origin_tool_call_id: invocation.call_id.clone(),
            agent: self.services.agent.clone(),
        };
        let request = CommandRequest {
            command: input.command,
            environment: input.environment,
            terminal_mode: input.terminal_mode,
        };
        let launcher = Arc::clone(&self.services.launcher);
        let security = security.clone();
        self.services
            .owner
            .start(process, report, move |lifetime| {
                Box::pin(async move { launcher.launch(request, security, lifetime).await })
            })
            .await
    }

    fn owner(&self, name: &str) -> io::Result<ResolvedProcess> {
        if name.contains('/') {
            self.services.resolver.resolve(name)
        } else {
            Ok(ResolvedProcess {
                owner: Arc::clone(&self.services.owner),
                name: name.into(),
            })
        }
    }

    async fn execute_inner(
        &self,
        invocation: &ProcessInvocation,
        security: &L::Security,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ProcessToolResult> {
        match self.kind {
            ProcessToolKind::ExecCommand => {
                let input = match ExecInput::parse(&invocation.arguments_json) {
                    Ok(input) => input,
                    Err(error) => return self.error(invocation, &error.to_string()),
                };
                if input.command.is_empty() {
                    return self.error(invocation, "no command given");
                }
                let name = input.name.as_deref().map(str::trim).map(str::to_owned);
                if name.as_deref() == Some("") {
                    return self.error(invocation, "process name must not be empty");
                }
                if let Some(target) = redundant_cd(&input.command, self.services.workspace.as_ref())
                {
                    return self.error(invocation, &format!("current directory is already '{target}'; do not change directory to conserve token."));
                }
                if leading_sleep(&input.command) {
                    return self.error(invocation, "leading 'sleep' is not allowed in exec_command; use the wait tool to pause instead.");
                }
                let reminder = (input.command.trim().contains('\n')
                    || input.command.encode_utf16().count() > NAMED_COMMAND_LENGTH)
                    && (name.is_none()
                        || input
                            .description
                            .as_deref()
                            .is_none_or(|value| value.trim().is_empty()));
                let process = self
                    .start(
                        invocation,
                        StartInput {
                            name,
                            command: input.command,
                            description: input.description.unwrap_or_default(),
                            environment: input.environment.into_iter().collect(),
                            terminal_mode: if input.tty {
                                TerminalMode::PseudoTerminal
                            } else {
                                TerminalMode::Pipe
                            },
                        },
                        security,
                        Arc::new(CompletionReport),
                    )
                    .await?;
                let outcome = process
                    .wait(
                        input.yield_after_ms.map(Duration::from_millis),
                        cancellation,
                    )
                    .await?;
                let mut text = outcome.format();
                if reminder {
                    text.push_str("\n\n");
                    text.push_str(&render(
                        &self.services.templates,
                        "exec-command.metadata-reminder",
                        &[("command_length", "300")],
                    )?);
                }
                Ok(ProcessToolResult {
                    text,
                    yielded_process: outcome.yielded_process,
                })
            }
            ProcessToolKind::WriteStdin => {
                let input = match WriteStdinInput::parse(&invocation.arguments_json) {
                    Ok(input) => input,
                    Err(error) => return self.error(invocation, &error.to_string()),
                };
                let name = input.name.trim();
                if name.is_empty() {
                    return self.error(invocation, "process name must not be empty");
                }
                let resolved = self.owner(name)?;
                let outcome = resolved
                    .owner
                    .claim(&resolved.name)
                    .await?
                    .write_stdin(
                        &input.input,
                        Duration::from_millis(input.yield_after_ms),
                        cancellation,
                    )
                    .await?;
                // The source wrapper returns formatted text, not a yielded-process envelope.
                Ok(ProcessToolResult::text(outcome.format()))
            }
            ProcessToolKind::InterruptProcess => {
                let input = match InterruptInput::parse(&invocation.arguments_json) {
                    Ok(input) => input,
                    Err(error) => return self.error(invocation, &error.to_string()),
                };
                let name = input.name.trim();
                if name.is_empty() {
                    return self.error(invocation, "process name must not be empty");
                }
                let resolved = self.owner(name)?;
                resolved
                    .owner
                    .claim(&resolved.name)
                    .await?
                    .send_signal(input.signal, cancellation)?;
                Ok(ProcessToolResult::text(format!(
                    "Signal {} sent to shell process '{}'.",
                    input.signal, resolved.name
                )))
            }
            ProcessToolKind::Monitor => {
                let input = match MonitorInput::parse(&invocation.arguments_json) {
                    Ok(input) => input,
                    Err(error) => return self.error(invocation, &error.to_string()),
                };
                if input.command.is_empty() {
                    return self.error(invocation, "no command given");
                }
                let description = input.description.unwrap_or_default();
                if description.trim().is_empty() {
                    return self.error(invocation, "description must not be empty");
                }
                let name = input.name.as_deref().map(str::trim).map(str::to_owned);
                if name.as_deref() == Some("") {
                    return self.error(invocation, "process name must not be empty");
                }
                if !(1_000..=1_800_000).contains(&input.timeout_ms) {
                    return self.error(
                        invocation,
                        "Tool argument 'timeout_ms' must be between 1000 and 1800000.",
                    );
                }
                if let Err(error) = input::validate_environment(&input.environment) {
                    return self.error(invocation, &error.to_string());
                }
                let report = Arc::new(MonitorProcessReport::new(
                    Duration::from_millis(input.timeout_ms as u64),
                    Arc::clone(&self.services.monitor_sink),
                ));
                let process = self
                    .start(
                        invocation,
                        StartInput {
                            name,
                            command: input.command,
                            description,
                            environment: input.environment.into_iter().collect(),
                            terminal_mode: TerminalMode::Pipe,
                        },
                        security,
                        report,
                    )
                    .await?;
                let yielded = process.yield_now();
                let stderr = yielded
                    .stderr_path
                    .as_ref()
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
                let text = render(
                    &self.services.templates,
                    "monitor.started",
                    &[("name", &yielded.name), ("stderr_path", &stderr)],
                )?;
                Ok(ProcessToolResult {
                    text,
                    yielded_process: Some(yielded),
                })
            }
        }
    }
}

impl<L: CapturedProcessLauncher<Request = CommandRequest>> Tool for ProcessTool<L>
where
    L::Security: Clone,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ProcessInvocation;
    type Selection = ();
    type Security = L::Security;
    type Output = ProcessToolResult;
    type Error = io::Error;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }

    fn parallel_safety(&self, invocation: &Self::Invocation) -> ParallelSafety {
        let parallel = self.kind == ProcessToolKind::ExecCommand
            && ExecInput::parse(&invocation.arguments_json).is_ok_and(|input| {
                !input.command.is_empty()
                    && !input
                        .name
                        .as_deref()
                        .is_some_and(|name| name.trim().is_empty())
                    && self.classifier.is_match(&input.command)
            });
        if parallel {
            ParallelSafety::ParallelSafe
        } else {
            ParallelSafety::Sequential
        }
    }

    fn enabled_after_interruption(&self) -> bool {
        self.kind == ProcessToolKind::InterruptProcess
    }

    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, Result<Self::Output, OperationError<Self::Error>>> {
        Box::pin(async move {
            match self
                .execute_inner(&invocation, context.security(), context.cancellation())
                .await
            {
                Ok(result) => Ok(result),
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && self.kind != ProcessToolKind::Monitor
                        && context.cancellation().is_cancelled() =>
                {
                    Err(OperationError::Cancelled)
                }
                Err(error) => self
                    .error(&invocation, &error.to_string())
                    .map_err(OperationError::Failed),
            }
        })
    }
}

struct StartInput {
    name: Option<String>,
    command: String,
    description: String,
    environment: Vec<(String, String)>,
    terminal_mode: TerminalMode,
}

fn render(
    templates: &PromptTemplateCatalog,
    key: &str,
    arguments: &[(&str, &str)],
) -> io::Result<String> {
    let arguments = arguments
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String((*value).to_owned())))
        .collect::<Vec<_>>();
    templates
        .render(key, &arguments, &NeverCancelled)
        .map_err(|error| io::Error::other(error.to_string()))
}

fn shell_space(value: char) -> bool {
    matches!(value, ' ' | '\t' | '\r' | '\n')
}

fn leading_sleep(command: &str) -> bool {
    command
        .trim_start_matches(shell_space)
        .strip_prefix("sleep")
        .is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(|value| shell_space(value) || matches!(value, ';' | '&'))
        })
}

fn redundant_cd(command: &str, workspace: &dyn ProcessWorkspace) -> Option<String> {
    let command = command.trim_start_matches(shell_space);
    let rest = command.strip_prefix("cd")?;
    if !rest.starts_with(shell_space) {
        return None;
    }
    let target = rest[rest.chars().next()?.len_utf8()..]
        .split([';', '\n', '\r', '&'])
        .next()?
        .trim();
    let target = if target.len() > 1 && target.starts_with('"') && target.ends_with('"') {
        &target[1..target.len() - 1]
    } else {
        target
    };
    if target.is_empty() {
        return None;
    }
    let expanded = match target {
        "." | "$WORKDIR" | "${WORKDIR}" => workspace.root().to_owned(),
        value if value.starts_with("$WORKDIR/") || value.starts_with("${WORKDIR}/") => value
            .replace("$WORKDIR/", &format!("{}/", workspace.root()))
            .replace("${WORKDIR}/", &format!("{}/", workspace.root())),
        value => value.to_owned(),
    };
    workspace
        .resolves_to_root(&expanded)
        .then(|| target.to_owned())
}
