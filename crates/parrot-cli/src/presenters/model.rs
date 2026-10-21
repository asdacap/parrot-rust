//! Typed tool presentation models and source redaction boundaries.

use std::{fmt, sync::Arc};

use parrot_domain::protocol::messages::parrot::{ArtifactReference, YieldedShellProcess};

use crate::{terminal_frames::ScrollbackLayout, terminal_layout::FinalFormatError};

pub type PresenterResult<T> = Result<T, PresenterError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresenterError(pub String);

impl fmt::Display for PresenterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PresenterError {}
impl From<serde_json::Error> for PresenterError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<FinalFormatError> for PresenterError {
    fn from(error: FinalFormatError) -> Self {
        Self(error.to_string())
    }
}

#[derive(Clone)]
pub struct ToolCallPresentation {
    pub tool_name: String,
    pub arguments_json: String,
    pub agent_reference_resolver: Arc<dyn Fn(&str) -> String + Send + Sync>,
}
impl ToolCallPresentation {
    pub fn new(tool_name: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments_json: arguments_json.into(),
            agent_reference_resolver: Arc::new(str::to_owned),
        }
    }
    pub fn resolve_agent_reference(&self, reference: &str) -> String {
        (self.agent_reference_resolver)(reference)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolTerminalStatus {
    #[default]
    Succeeded,
    Cancelled,
    Errored,
    ReportedFailure,
}

#[derive(Clone, Debug, Default)]
pub struct ToolTerminalPresentation {
    pub status: ToolTerminalStatus,
    pub result_present: bool,
    pub result: String,
    pub error: String,
    pub yielded_process: Option<YieldedShellProcess>,
    pub artifacts: Vec<ArtifactReference>,
    pub start_omitted: bool,
}
impl ToolTerminalPresentation {
    pub fn resolve_status(&self) -> ToolTerminalStatus {
        if self.result_present && self.result.starts_with("error: ") {
            ToolTerminalStatus::ReportedFailure
        } else {
            self.status
        }
    }
    pub fn resolve_process_status(&self) -> ToolTerminalStatus {
        let status = self.resolve_status();
        if status != self.status || !self.result_present {
            return status;
        }
        let Some(value) = self.result.strip_prefix("Process exited with code ") else {
            return status;
        };
        let end = value.find([' ', '\n']).unwrap_or(value.len());
        let suffix = &value[end..];
        if (suffix.is_empty() || suffix.starts_with('\n') || suffix.starts_with(" after "))
            && value[..end].parse::<i32>().is_ok_and(|code| code != 0)
        {
            ToolTerminalStatus::ReportedFailure
        } else {
            status
        }
    }
    pub fn describe_block(&self, success_kind: ToolBlockKind) -> ToolBlock {
        if matches!(
            self.resolve_status(),
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure
        ) {
            return ToolBlock::from_error(if self.result_present {
                &self.result
            } else {
                &self.error
            });
        }
        if !self.result_present || self.result.is_empty() {
            return ToolBlock::empty();
        }
        match success_kind {
            ToolBlockKind::Diff => ToolBlock::from_diff(&self.result),
            ToolBlockKind::Text => ToolBlock::from_text(&self.result),
            _ => ToolBlock::empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolPresentationStyle {
    #[default]
    Default,
    Muted,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolPresentationMetadata {
    pub style: ToolPresentationStyle,
    pub success_icon: String,
    pub live_only: bool,
    pub terminal_only: bool,
    pub modeline: bool,
    pub redacted_input_fields: Vec<String>,
    pub suppress_terminal_details: bool,
    pub multiline_label: bool,
    pub defer_started: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolBlockKind {
    #[default]
    None,
    Text,
    Status,
    Output,
    Diff,
    Queue,
    CompletedInput,
    Error,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolBlock {
    pub kind: ToolBlockKind,
    pub text: String,
}
impl ToolBlock {
    pub fn new(kind: ToolBlockKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
    pub fn empty() -> Self {
        Self::default()
    }
    fn nonempty(kind: ToolBlockKind, text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            Self::empty()
        } else {
            Self { kind, text }
        }
    }
    pub fn from_text(text: impl Into<String>) -> Self {
        Self::nonempty(ToolBlockKind::Text, text)
    }
    pub fn from_status(text: impl Into<String>) -> Self {
        Self::nonempty(ToolBlockKind::Status, text)
    }
    pub fn from_output(text: impl Into<String>) -> Self {
        Self::nonempty(ToolBlockKind::Output, text)
    }
    pub fn from_diff(text: impl Into<String>) -> Self {
        Self::new(ToolBlockKind::Diff, text)
    }
    pub fn from_queue(text: impl Into<String>) -> Self {
        Self::nonempty(ToolBlockKind::Queue, text)
    }
    pub fn from_completed_input(text: impl Into<String>) -> Self {
        Self::new(ToolBlockKind::CompletedInput, text)
    }
    pub fn from_error(text: impl Into<String>) -> Self {
        Self::nonempty(ToolBlockKind::Error, text)
    }
    pub fn from_details(values: impl IntoIterator<Item = String>) -> Self {
        Self::from_text(
            values
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}
#[derive(Clone)]
pub struct ToolReport {
    pub label: String,
    pub status: Option<ToolTerminalStatus>,
    pub block: ToolBlock,
    pub metadata: ToolPresentationMetadata,
    pub frame: usize,
    pub timer: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}
impl fmt::Debug for ToolReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolReport")
            .field("label", &self.label)
            .field("status", &self.status)
            .field("block", &self.block)
            .field("metadata", &self.metadata)
            .field("frame", &self.frame)
            .field("timer", &self.timer.is_some())
            .finish()
    }
}
impl ToolReport {
    pub fn live(
        label: impl Into<String>,
        block: ToolBlock,
        metadata: ToolPresentationMetadata,
        frame: usize,
    ) -> Self {
        let label = label.into();
        let label = if metadata.multiline_label {
            crate::terminal_layout::TerminalText::sanitize(&label)
        } else {
            super::rendering::display_label(&label)
        };
        Self {
            label,
            status: None,
            block,
            metadata,
            frame,
            timer: None,
        }
    }
    pub fn terminal(
        label: impl Into<String>,
        block: ToolBlock,
        status: ToolTerminalStatus,
        metadata: ToolPresentationMetadata,
    ) -> Self {
        let mut report = Self::live(label, block, metadata, 0);
        report.status = Some(status);
        report
    }
}
#[derive(Clone, Debug)]
pub enum Presentation {
    Tool(ToolReport),
    Diff {
        tool_name: String,
        path: String,
        diff: String,
    },
    AgentSpawn {
        name: String,
        metadata: String,
        prompt: String,
    },
    Lines {
        lines: Vec<String>,
        layout: ScrollbackLayout,
    },
}

impl Presentation {
    pub fn animate(&self, frame: usize) -> Self {
        let mut animated = self.clone();
        if let Self::Tool(report) = &mut animated {
            report.frame = frame;
        }
        animated
    }
}

pub trait ToolPresenter: Send + Sync {
    fn tool_name(&self) -> &str;
    fn metadata(&self) -> ToolPresentationMetadata {
        ToolPresentationMetadata::default()
    }
    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation>;
    fn present_started(
        &self,
        _call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        Ok(None)
    }
    fn present_child_started(
        &self,
        _call: &ToolCallPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        Ok(None)
    }
    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>>;
}

pub fn redact_call(
    call: &ToolCallPresentation,
    metadata: &ToolPresentationMetadata,
) -> ToolCallPresentation {
    let mut redacted = call.clone();
    if metadata.redacted_input_fields.is_empty() {
        return redacted;
    }
    redacted.arguments_json =
        redact_arguments(&call.arguments_json, metadata).unwrap_or_else(|| "<redacted>".to_owned());
    redacted
}

// Stream the root members to retain source property order without enabling a
// second JSON representation or reordering a generic fallback's visible YAML.
fn redact_arguments(source: &str, metadata: &ToolPresentationMetadata) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(source).ok()?;
    root.as_object()?;
    let mut rest = source.trim_start().strip_prefix('{')?.trim_start();
    let mut fields = Vec::new();
    loop {
        if rest.starts_with('}') {
            break;
        }
        let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let key = keys.next()?.ok()?;
        rest = rest[keys.byte_offset()..]
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
        let value = values.next()?.ok()?;
        let serialized = if metadata.redacted_input_fields.contains(&key) {
            serde_json::to_string(&match value.as_str() {
                Some(text) => format!("<redacted: {} chars>", text.chars().count()),
                None => "<redacted>".to_owned(),
            })
            .ok()?
        } else {
            rest[..values.byte_offset()].to_owned()
        };
        fields.push(format!(
            "{}:{}",
            serde_json::to_string(&key).ok()?,
            serialized
        ));
        rest = rest[values.byte_offset()..].trim_start();
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail.trim_start();
        } else {
            break;
        }
    }
    Some(format!("{{{}}}", fields.join(",")))
}

pub fn redact_terminal(
    terminal: &ToolTerminalPresentation,
    metadata: &ToolPresentationMetadata,
) -> ToolTerminalPresentation {
    let mut redacted = terminal.clone();
    if metadata.suppress_terminal_details {
        redacted.result_present = false;
        redacted.result.clear();
        redacted.error.clear();
    }
    redacted
}
