use crate::presenters::*;
use parrot_tools::sandbox::linux::ReadOnlyExecCommandClassifier;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct ExecCommandToolPresenter {
    classifier: ReadOnlyExecCommandClassifier,
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
}
impl Default for ExecCommandToolPresenter {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
impl ExecCommandToolPresenter {
    pub fn new(prefixes: Vec<String>) -> Self {
        let epoch = Instant::now();
        Self::with_clock(prefixes, Arc::new(move || epoch.elapsed()))
    }
    pub fn with_clock(
        prefixes: Vec<String>,
        clock: Arc<dyn Fn() -> Duration + Send + Sync>,
    ) -> Self {
        Self {
            classifier: ReadOnlyExecCommandClassifier::new(prefixes),
            clock,
        }
    }
    pub fn summarize(name: Option<&str>, description: Option<&str>, command: &str) -> String {
        if is_named_long(name, description, command) {
            format!(
                "{} · {}",
                name.unwrap_or_default(),
                description.unwrap_or_default()
            )
        } else {
            command.into()
        }
    }
    fn metadata_for(&self, read_only: bool) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            style: if read_only {
                ToolPresentationStyle::Muted
            } else {
                ToolPresentationStyle::Default
            },
            ..self.metadata()
        }
    }
}
impl ToolPresenter for ExecCommandToolPresenter {
    fn tool_name(&self) -> &str {
        "exec_command"
    }
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata {
            multiline_label: true,
            defer_started: true,
            ..Default::default()
        }
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        let args = Arguments::parse(&call.arguments_json)?;
        let read_only = self.classifier.is_match(&args.command);
        let mut report = ToolReport::live(
            args.label(),
            ToolBlock::empty(),
            self.metadata_for(read_only),
            frame,
        );
        if !read_only {
            let clock = Arc::clone(&self.clock);
            let started = clock();
            report.timer = Some(Arc::new(move || {
                format!(
                    "running {}",
                    duration_text(clock().saturating_sub(started).as_secs())
                )
            }));
        }
        Ok(Presentation::Tool(report))
    }
    fn present_started(
        &self,
        call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        let args = Arguments::parse(&call.arguments_json)?;
        Ok(
            if is_named_long(
                args.name.as_deref(),
                args.description.as_deref(),
                &args.command,
            ) {
                let mut metadata = self.metadata_for(self.classifier.is_match(&args.command));
                metadata.success_icon = "○".into();
                Some(Presentation::Tool(ToolReport::terminal(
                    args.label_with_command(),
                    ToolBlock::empty(),
                    ToolTerminalStatus::Succeeded,
                    metadata,
                )))
            } else {
                None
            },
        )
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        if terminal.yielded_process.is_some() {
            return Ok(None);
        }
        let args = Arguments::parse(&call.arguments_json)?;
        let read_only = self.classifier.is_match(&args.command);
        let label = if terminal.start_omitted {
            args.label_with_command()
        } else {
            args.label()
        };
        let status = terminal.resolve_process_status();
        let output = if matches!(
            status,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            if terminal.result_present {
                without_lone_stdout_label(&terminal.result)
            } else {
                terminal.error.clone()
            }
        } else if read_only && status == ToolTerminalStatus::Succeeded {
            String::new()
        } else {
            without_lone_stdout_label(&terminal.result)
        };
        Ok(Some(Presentation::Tool(ToolReport::terminal(
            label,
            ToolBlock::from_output(tail(&output, 10)),
            status,
            self.metadata_for(read_only),
        ))))
    }
}
struct Arguments {
    command: String,
    name: Option<String>,
    description: Option<String>,
}
impl Arguments {
    fn parse(json: &str) -> PresenterResult<Self> {
        let value: Value = serde_json::from_str(json)?;
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| value.get("cmd").and_then(Value::as_str))
            .ok_or_else(|| PresenterError("exec_command requires a string command.".into()))?;
        Ok(Self {
            command: command.into(),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            description: value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
    fn label(&self) -> String {
        format!(
            "$ {}",
            ExecCommandToolPresenter::summarize(
                self.name.as_deref(),
                self.description.as_deref(),
                &self.command
            )
        )
    }
    fn label_with_command(&self) -> String {
        format!("{}\n{}", self.label(), self.command)
    }
}
fn is_named_long(name: Option<&str>, description: Option<&str>, command: &str) -> bool {
    command.encode_utf16().count() > 200
        && name.is_some()
        && description.is_some_and(|description| !description.trim().is_empty())
}
fn without_lone_stdout_label(result: &str) -> String {
    if result.contains("\n[stderr]\n") {
        return result.into();
    }
    result.replacen("\n[stdout]\n", "\n", 1)
}
fn tail(value: &str, maximum: usize) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let lines = normalized
        .trim_end_matches('\n')
        .split('\n')
        .collect::<Vec<_>>();
    lines[lines.len().saturating_sub(maximum)..].join("\n")
}
fn duration_text(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}
