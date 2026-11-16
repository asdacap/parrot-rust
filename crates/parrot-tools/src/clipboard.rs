//! Clipboard copy through the terminal's OSC 52 channel. The tool validates and
//! encodes the text, then publishes a `clipboard_write` notice; the user's
//! terminal application (the CLI) performs the actual sequence write.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use parrot_core::NeverCancelled;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        events::EventPublisher,
        security::{AccessSnapshot, InvocationScope},
        storage::Committed,
        tools::{ParallelSafety, Tool},
    },
    protocol::messages::parrot::{AgentNotice, ClipboardWrite, Event},
    values::{identifiers::Identifier, tools::ConfiguredToolDefinition},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::conversation::{AgentHistoryView, ConversationOrigin};
use protobuf::Message;
use serde::Deserialize;
use serde_json::Value;

use crate::session::{Failure, FailureKind};

/// The `AgentNotice` kind the CLI listens for. The notice text stays empty so
/// renderers that do not know the kind show nothing.
pub const CLIPBOARD_NOTICE_KIND: &str = "clipboard_write";

const MAXIMUM_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Invocation {
    pub call_id: String,
    pub arguments_json: String,
    pub prompt_templates: Arc<PromptTemplateCatalog>,
}

#[derive(Deserialize)]
struct CopyInput {
    text: String,
}

/// The one retained agent's clipboard publisher. Appends the notice through the
/// agent's history before publication, so the committed record survives replay
/// while the CLI still decides when the sequence actually reaches the terminal.
pub struct ClipboardCopyTool<S, P: EventPublisher<Event = Event>> {
    definition: ConfiguredToolDefinition,
    history: AgentHistoryView,
    publisher: Arc<P>,
    marker: std::marker::PhantomData<fn() -> S>,
}

impl<S, P> ClipboardCopyTool<S, P>
where
    S: AccessSnapshot + 'static,
    P: EventPublisher<Event = Event>,
    P::Error: std::fmt::Display,
{
    pub fn new(
        definition: ConfiguredToolDefinition,
        history: AgentHistoryView,
        publisher: Arc<P>,
    ) -> Self {
        Self {
            definition,
            history,
            publisher,
            marker: std::marker::PhantomData,
        }
    }

    pub fn name(&self) -> &'static str {
        "clipboard_copy"
    }

    async fn copy(
        &self,
        invocation: &Invocation,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, Failure> {
        let text = parse_text(&invocation.arguments_json)?;
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let event = notice_event(&self.history, &text)?;
        self.history
            .append_message(event.clone(), None, ConversationOrigin::Model)
            .await
            .map_err(|error| {
                OperationError::Failed(Failure::new(
                    FailureKind::Other,
                    format!("clipboard notice append failed: {error}"),
                ))
            })?;
        let committed = Committed::after_commit(event);
        if let Err(error) = self
            .publisher
            .publish_committed(&committed, cancellation)
            .await
        {
            let detail = match error {
                OperationError::Cancelled => return Err(OperationError::Cancelled),
                OperationError::Failed(error) => error,
            };
            return Err(OperationError::Failed(Failure::new(
                FailureKind::Other,
                format!("clipboard notice publication failed: {detail}"),
            )));
        }
        render(
            &invocation.prompt_templates,
            "clipboard-tool.copied",
            &[("bytes", text.len().to_string())],
        )
    }

    fn catch(
        &self,
        invocation: &Invocation,
        result: OperationResult<String, Failure>,
    ) -> OperationResult<String, Failure> {
        match result {
            Err(OperationError::Failed(error))
                if matches!(
                    error.kind,
                    FailureKind::Json | FailureKind::Format | FailureKind::Argument
                ) =>
            {
                error_output(invocation, &error.message)
            }
            result => result,
        }
    }
}

fn parse_text(arguments_json: &str) -> OperationResult<String, Failure> {
    let input: CopyInput = serde_json::from_str(arguments_json).map_err(|error| {
        OperationError::Failed(Failure::new(
            FailureKind::Json,
            format!("clipboard_copy requires a text string: {error}"),
        ))
    })?;
    if input.text.is_empty() {
        return Err(OperationError::Failed(Failure::new(
            FailureKind::Argument,
            "clipboard_copy requires a nonempty text.",
        )));
    }
    if input.text.len() > MAXIMUM_TEXT_BYTES {
        return Err(OperationError::Failed(Failure::new(
            FailureKind::Argument,
            format!(
                "text is {} bytes; the clipboard cap is {MAXIMUM_TEXT_BYTES} bytes.",
                input.text.len()
            ),
        )));
    }
    Ok(input.text)
}

fn notice_event(history: &AgentHistoryView, text: &str) -> OperationResult<Event, Failure> {
    let mut event = Event::new();
    event.agent_session_id = history.agent_session_id().to_owned();
    let mut data = ClipboardWrite::new();
    data.base64_text = STANDARD.encode(text.as_bytes());
    event.set_agent_notice(AgentNotice {
        kind: CLIPBOARD_NOTICE_KIND.to_owned(),
        notice: String::new(),
        data: data.write_to_bytes().map_err(|error| {
            OperationError::Failed(Failure::new(FailureKind::Other, error.to_string()))
        })?,
        ..AgentNotice::new()
    });
    event.id = Identifier::event_id().map_err(|error| {
        OperationError::Failed(Failure::new(FailureKind::Other, error.to_string()))
    })?;
    Ok(event)
}

impl<S, P> Tool for ClipboardCopyTool<S, P>
where
    S: AccessSnapshot + 'static,
    P: EventPublisher<Event = Event>,
    P::Error: std::fmt::Display,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = ();
    type Security = S;
    type Output = String;
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
        scope: InvocationScope<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        Box::pin(async move {
            self.catch(
                &invocation,
                self.copy(&invocation, scope.cancellation()).await,
            )
        })
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
        .render(id, &arguments, &NeverCancelled)
        .map_err(|error| {
            OperationError::Failed(Failure::new(FailureKind::Argument, error.to_string()))
        })
}

fn error_output(invocation: &Invocation, message: &str) -> OperationResult<String, Failure> {
    render(
        &invocation.prompt_templates,
        "tool-result.error",
        &[("message", message.to_owned())],
    )
}
