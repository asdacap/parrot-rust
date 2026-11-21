//! ACP (Agent Client Protocol) v1 adapter serving [`ParrotService`] to editor
//! clients.
//!
//! The adapter runs a JSON-RPC connection over newline-delimited messages on
//! the transport the caller supplies; the `parrot acp` command passes stdio.
//! Only this adapter may write to stdout, so process logs go to stderr.
//!
//! Session identity: the ACP `sessionId` is the Parrot user session id, so a
//! session round-trips without a translation table.
//!
//! Event-kind to `session/update` mapping, checked against
//! `crates/parrot-core/src/protocol/event.rs` and the wire payloads in
//! `proto/parrot.proto`:
//!
//! | Parrot event | ACP update |
//! | --- | --- |
//! | `TextChunk` | `agent_message_chunk` |
//! | `ReasoningChunk` | `agent_thought_chunk` |
//! | `ToolStarted` | `tool_call` with status `in_progress` |
//! | `ToolFinished` | `tool_call_update` with status `completed` and the result as content |
//! | `ToolError` | `tool_call_update` with status `failed` and the message as content |
//! | `ToolCancelled` | `tool_call_update` with status `failed` (ACP has no cancelled status) |
//! | `TurnEnded` | records the stop reason; no update |
//! | `TurnFailed` | ends the prompt with a JSON-RPC error |
//! | `ModeTurnCompleted` | ends the prompt with the recorded stop reason |
//! | `InputCanceled` | ends the prompt with stop reason `cancelled` |
//! | `PermissionPending` | `session/request_permission`; the reply feeds the existing permission flow |
//! | everything else | no update |
//!
//! Stop reasons, from the `TurnEnded` `finish_reason` strings in
//! `crates/parrot-provider/src/sessions/core_protocol.rs`: `interrupted` maps
//! to `cancelled`, `length` and `incomplete` map to `max_tokens`,
//! `content_filter` maps to `refusal`, and everything else maps to `end_turn`.
//!
//! First-cut scope cuts: the client's `cwd`, `additionalDirectories`, and
//! `mcpServers` are ignored because the host resolves its workspace from the
//! process launch directory and configures MCP itself; `session/load`,
//! `session/resume`, and session modes are unsupported and unadvertised; tool
//! argument streaming (`ToolCallChunk`) is not reconstructed into `rawInput`.

pub mod filesystem;

use crate::composition::protocol::{ParrotService, RpcCode, RpcError};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    Agent, Client, ConnectTo, Error, Stdio, on_receive_notification, on_receive_request, v1,
};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_domain::contracts::filesystem::{FileReadError, FileReadErrorKind};
use parrot_domain::protocol::messages::parrot as wire;
use parrot_runtime::agent_turn::lifecycle::Signal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use v1::schema::{ReadTextFileRequest, WriteTextFileRequest};
use v1::{ConnectionTo as V1ConnectionTo, Responder as V1Responder};

/// Serves one ACP connection against the composed local service.
pub struct AcpAgent {
    service: Arc<ParrotService>,
    /// Session defaults the owner supplies; the adapter forces interactive
    /// permissions because an ACP client always interacts.
    new_session: wire::CreateSessionRequest,
    /// What the client advertised at initialize. The file tools use the fs
    /// entries to decide whether reads and writes route through the client.
    client_capabilities: Mutex<Option<v1::schema::ClientCapabilities>>,
    /// Client file access handles for the sessions this connection created.
    /// Dropping them removes the registry entries, so a disconnected editor
    /// falls back to the local file path.
    registrations: Mutex<Vec<filesystem::Registration>>,
}

impl AcpAgent {
    pub fn new(service: Arc<ParrotService>, new_session: wire::CreateSessionRequest) -> Self {
        Self {
            service,
            new_session,
            client_capabilities: Mutex::new(None),
            registrations: Mutex::new(Vec::new()),
        }
    }

    /// The fs capabilities the connected client advertised, or an empty set
    /// before initialize.
    pub fn client_filesystem_capabilities(&self) -> v1::schema::FileSystemCapabilities {
        self.client_capabilities
            .lock()
            .expect("acp capabilities poisoned")
            .as_ref()
            .map_or_else(
                v1::schema::FileSystemCapabilities::default,
                |capabilities| capabilities.fs.clone(),
            )
    }

    /// Runs the ACP connection over process stdio until the editor closes
    /// stdin. Stdout carries only JSON-RPC; logs go to stderr.
    pub async fn run_stdio(self) -> agent_client_protocol::Result<()> {
        self.run(Stdio::new()).await
    }

    /// Runs the ACP connection until the transport closes.
    pub async fn run(self, transport: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        let state = Arc::new(self);
        Agent
            .v1()
            .name("parrot")
            .on_receive_request(
                {
                    let state = state.clone();
                    async move |request: InitializeRequest,
                                responder: V1Responder<InitializeResponse>,
                                _| {
                        *state
                            .client_capabilities
                            .lock()
                            .expect("acp capabilities poisoned") =
                            Some(request.client_capabilities.clone());
                        responder.respond(initialize_response(&request))
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state.clone();
                    async move |request: NewSessionRequest,
                                responder: V1Responder<NewSessionResponse>,
                                cx: V1ConnectionTo<Client>| {
                        responder.respond(new_session(&state, &request, cx).await?)
                    }
                },
                on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = state.clone();
                    async move |request: PromptRequest,
                                responder: V1Responder<PromptResponse>,
                                cx: V1ConnectionTo<Client>| {
                        match prompt_text(&request.prompt) {
                            Some(text) => {
                                cx.spawn(run_prompt(
                                    state.clone(),
                                    cx.clone(),
                                    request.session_id.clone(),
                                    text,
                                    responder,
                                ))?;
                                Ok(())
                            }
                            None => responder.respond_with_error(
                                Error::invalid_params().data("the prompt carries no text"),
                            ),
                        }
                    }
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let state = state.clone();
                    async move |notification: CancelNotification, _cx: V1ConnectionTo<Client>| {
                        cancel(&state, &notification.session_id).await;
                        Ok(())
                    }
                },
                on_receive_notification!(),
            )
            .connect_to(transport)
            .await
    }
}

fn initialize_response(request: &InitializeRequest) -> InitializeResponse {
    // Only v1 is supported. Echo the client's v1 request, and offer v1 for
    // anything else so a client that cannot speak v1 disconnects.
    let version = if request.protocol_version == ProtocolVersion::V1 {
        request.protocol_version
    } else {
        ProtocolVersion::V1
    };
    InitializeResponse::new(version)
        .agent_capabilities(AgentCapabilities::new())
        .agent_info(Implementation::new("parrot", env!("CARGO_PKG_VERSION")))
}

async fn new_session(
    state: &AcpAgent,
    _request: &NewSessionRequest,
    connection: V1ConnectionTo<Client>,
) -> Result<NewSessionResponse, Error> {
    let mut create = state.new_session.clone();
    create.interactive_permissions = true;
    let session = state
        .service
        .create_session(&create, Arc::new(Signal::new()))
        .await
        .map_err(|error| service_error(error, "create_session"))?;
    let capabilities = state.client_filesystem_capabilities();
    if capabilities.read_text_file || capabilities.write_text_file {
        let access = AcpClientFileAccess {
            connection,
            session_id: SessionId::from(session.id.clone()),
            can_read: capabilities.read_text_file,
            can_write: capabilities.write_text_file,
        };
        state
            .registrations
            .lock()
            .expect("acp registrations poisoned")
            .push(
                filesystem::AcpFileRegistry::shared()
                    .register(session.id.clone(), Arc::new(access)),
            );
    }
    Ok(NewSessionResponse::new(session.id))
}

/// Moves file bytes between Parrot's file tools and the connected ACP client.
/// Registration is gated on the fs capabilities the client advertised at
/// initialize, so an editor without the capability never receives calls.
struct AcpClientFileAccess {
    connection: V1ConnectionTo<Client>,
    session_id: SessionId,
    can_read: bool,
    can_write: bool,
}

impl filesystem::ClientFileAccess for AcpClientFileAccess {
    fn read_text_file<'a>(
        &'a self,
        path: PathBuf,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, FileReadError>> {
        Box::pin(async move {
            if !self.can_read {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::NotSupported,
                    "the acp client does not serve text file reads",
                )));
            }
            if cancellation.check().is_err() {
                return Err(OperationError::Cancelled);
            }
            let request = ReadTextFileRequest::new(self.session_id.clone(), path);
            match self.connection.send_request(request).block_task().await {
                Ok(response) => Ok(response.content),
                Err(error) => {
                    if cancellation.is_cancelled() {
                        return Err(OperationError::Cancelled);
                    }
                    Err(OperationError::Failed(filesystem::transfer_failed(error)))
                }
            }
        })
    }

    fn write_text_file<'a>(
        &'a self,
        path: PathBuf,
        content: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), FileReadError>> {
        Box::pin(async move {
            if !self.can_write {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::NotSupported,
                    "the acp client does not serve text file writes",
                )));
            }
            if cancellation.check().is_err() {
                return Err(OperationError::Cancelled);
            }
            let request = WriteTextFileRequest::new(self.session_id.clone(), path, content);
            match self.connection.send_request(request).block_task().await {
                Ok(_) => Ok(()),
                Err(error) => {
                    if cancellation.is_cancelled() {
                        return Err(OperationError::Cancelled);
                    }
                    Err(OperationError::Failed(filesystem::transfer_failed(error)))
                }
            }
        })
    }
}

async fn cancel(state: &AcpAgent, session_id: &SessionId) {
    let request = wire::InterruptRequest {
        user_session_id: session_id.to_string(),
        ..Default::default()
    };
    if let Err(error) = state
        .service
        .interrupt(&request, Arc::new(Signal::new()))
        .await
    {
        eprintln!("parrot acp: interrupt failed: {error}");
    }
}

async fn run_prompt(
    state: Arc<AcpAgent>,
    cx: V1ConnectionTo<Client>,
    session_id: SessionId,
    text: String,
    responder: V1Responder<PromptResponse>,
) -> agent_client_protocol::Result<()> {
    let user_session_id = session_id.to_string();
    let cancellation = Arc::new(Signal::new());
    // Subscribe before sending so no turn event can fall between admission
    // and the stream opening.
    let listen = wire::ListenRequest {
        user_session_id: user_session_id.clone(),
        replay: false,
        ..Default::default()
    };
    let mut stream = state
        .service
        .listen(&listen, cancellation.clone())
        .map_err(|error| service_error(error, "listen"))?;
    let send = wire::SendMessageRequest {
        user_session_id: user_session_id.clone(),
        text,
        delivery: wire::Delivery::DELIVERY_QUEUE.into(),
        ..Default::default()
    };
    if let Err(error) = state
        .service
        .send_message(&send, cancellation.clone())
        .await
    {
        if error.code == RpcCode::Cancelled {
            return cancelled_prompt(responder);
        }
        return responder.respond_with_error(service_error(error, "send_message"));
    }

    let mut finish_reason: Option<String> = None;
    loop {
        let event = match stream.next(cancellation.as_ref()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                return respond_stop(
                    responder,
                    finish_reason
                        .as_deref()
                        .map_or(StopReason::EndTurn, |reason| {
                            if reason == "interrupted" {
                                StopReason::Cancelled
                            } else {
                                StopReason::EndTurn
                            }
                        }),
                );
            }
            Err(error) => {
                return responder.respond_with_error(service_error(error, "listen stream"));
            }
        };
        match event.payload {
            Some(wire::event::Payload::TurnEnded(ended)) => {
                finish_reason = Some(ended.finish_reason.clone());
                if ended.finish_reason == "interrupted" {
                    return respond_stop(responder, StopReason::Cancelled);
                }
            }
            Some(wire::event::Payload::ModeTurnCompleted(_)) => {
                return respond_stop(
                    responder,
                    finish_reason
                        .as_deref()
                        .map_or(StopReason::EndTurn, map_stop_reason),
                );
            }
            Some(wire::event::Payload::TurnFailed(failed)) => {
                return responder.respond_with_error(Error::internal_error().data(failed.message));
            }
            Some(wire::event::Payload::InputCanceled(_)) => {
                return respond_stop(responder, StopReason::Cancelled);
            }
            Some(wire::event::Payload::PermissionPending(pending)) => {
                let choice = request_permission(&cx, &session_id, &pending, &cancellation).await;
                let reply = wire::ReplyPermissionRequest {
                    user_session_id: user_session_id.clone(),
                    permission_request_id: pending.id.clone(),
                    choice_value: choice.to_owned(),
                    // ACP selections carry no reason; Parrot's requires_reason
                    // choice is not offered over ACP.
                    reason: String::new(),
                    ..Default::default()
                };
                if let Err(error) = state.service.reply_permission(&reply).await {
                    eprintln!("parrot acp: permission reply failed: {error}");
                }
            }
            _ => {
                if let Some(update) = map_event(&event) {
                    cx.send_notification(SessionNotification::new(session_id.clone(), update))?;
                }
            }
        }
    }
}

fn respond_stop(
    responder: V1Responder<PromptResponse>,
    stop_reason: StopReason,
) -> agent_client_protocol::Result<()> {
    responder.respond(PromptResponse::new(stop_reason))
}

/// Presents one pending permission to the ACP client and returns the Parrot
/// choice value for the selected outcome. ACP offers four permission kinds
/// while Parrot decides grant versus reject and owns any remembering, so both
/// allow kinds reply `grant` and both reject kinds reply `reject`. The
/// permission request id doubles as the ACP tool-call id, with the reason as
/// the title, because the wire contract carries no tool-call linkage. A client
/// failure or cancellation rejects, which keeps the turn moving.
async fn request_permission(
    cx: &V1ConnectionTo<Client>,
    session_id: &SessionId,
    pending: &wire::PendingPermission,
    cancellation: &Arc<Signal>,
) -> &'static str {
    let options = vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow_always",
            "Allow always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            "reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            "reject_always",
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ];
    let tool_call = ToolCallUpdate::new(
        pending.id.clone(),
        ToolCallUpdateFields::new()
            .title(pending.reason.clone())
            .kind(ToolKind::Edit)
            .status(ToolCallStatus::Pending),
    );
    let request = RequestPermissionRequest::new(session_id.clone(), tool_call, options);
    let response = tokio::select! {
        response = cx.send_request(request).block_task() => response,
        _ = cancellation.cancelled() => return "reject",
    };
    match response {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                permission_choice(selected.option_id.0.as_ref())
            }
            RequestPermissionOutcome::Cancelled => "reject",
            _ => "reject",
        },
        Err(error) => {
            eprintln!("parrot acp: permission request failed: {error}");
            "reject"
        }
    }
}

fn permission_choice(option_id: &str) -> &'static str {
    match option_id {
        "allow_once" | "allow_always" => "grant",
        _ => "reject",
    }
}

fn cancelled_prompt(responder: V1Responder<PromptResponse>) -> agent_client_protocol::Result<()> {
    responder.respond(PromptResponse::new(StopReason::Cancelled))
}

fn service_error(error: RpcError, operation: &str) -> Error {
    Error::internal_error().data(format!("{operation}: {}", error.message))
}

/// Extracts the prompt text an ACP client sent. Text blocks join with
/// newlines, resource links contribute `name (uri)`, and embedded text
/// resources contribute their text. Image, audio, and blob resources are
/// unsupported and dropped.
fn prompt_text(prompt: &[ContentBlock]) -> Option<String> {
    let mut text = Vec::new();
    for block in prompt {
        match block {
            ContentBlock::Text(content) => text.push(content.text.clone()),
            ContentBlock::ResourceLink(link) => text.push(format!("{} ({})", link.name, link.uri)),
            ContentBlock::Resource(resource) => {
                if let v1::schema::EmbeddedResourceResource::TextResourceContents(contents) =
                    &resource.resource
                {
                    text.push(contents.text.clone());
                }
            }
            ContentBlock::Image(_) | ContentBlock::Audio(_) => {}
            _ => {}
        }
    }
    (!text.is_empty()).then(|| text.join("\n"))
}

fn map_stop_reason(finish_reason: &str) -> StopReason {
    match finish_reason {
        "interrupted" => StopReason::Cancelled,
        "length" | "incomplete" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

fn map_event(event: &wire::Event) -> Option<SessionUpdate> {
    let payload = event.payload.as_ref()?;
    Some(match payload {
        wire::event::Payload::TextChunk(chunk) => SessionUpdate::AgentMessageChunk(
            v1::schema::ContentChunk::new(text_block(&chunk.fragment)),
        ),
        wire::event::Payload::ReasoningChunk(chunk) => SessionUpdate::AgentThoughtChunk(
            v1::schema::ContentChunk::new(text_block(&chunk.fragment)),
        ),
        wire::event::Payload::ToolStarted(started) => SessionUpdate::ToolCall(
            ToolCall::new(started.tool_call_id.clone(), tool_title(started))
                .kind(tool_kind(&started.tool_name))
                .status(ToolCallStatus::InProgress),
        ),
        wire::event::Payload::ToolFinished(finished) => {
            let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
            if let Some(result) = finished
                .display_result
                .as_ref()
                .or(finished.result.as_ref())
            {
                fields = fields.content(vec![ToolCallContent::from(text_block(result))]);
            }
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                finished.tool_call_id.clone(),
                fields,
            ))
        }
        wire::event::Payload::ToolError(error) => {
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                error.tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![ToolCallContent::from(text_block(&error.message))]),
            ))
        }
        wire::event::Payload::ToolCancelled(cancelled) => {
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                cancelled.tool_call_id.clone(),
                ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
            ))
        }
        _ => return None,
    })
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn tool_title(started: &wire::ToolStarted) -> String {
    started.path_variables.get("path").map_or_else(
        || started.tool_name.clone(),
        |path| format!("{} {path}", started.tool_name),
    )
}

fn tool_kind(tool_name: &str) -> ToolKind {
    let name = tool_name.to_ascii_lowercase();
    if name.contains("read") || name.contains("list") || name.contains("view") {
        ToolKind::Read
    } else if name.contains("edit") || name.contains("write") || name.contains("patch") {
        ToolKind::Edit
    } else if name.contains("delete") || name.contains("remove") {
        ToolKind::Delete
    } else if name.contains("move") || name.contains("rename") {
        ToolKind::Move
    } else if name.contains("search") || name.contains("grep") || name.contains("find") {
        ToolKind::Search
    } else if name.contains("shell")
        || name.contains("bash")
        || name.contains("exec")
        || name.contains("process")
    {
        ToolKind::Execute
    } else if name.contains("fetch") || name.contains("http") || name.contains("web") {
        ToolKind::Fetch
    } else {
        ToolKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use v1::schema::ImageContent;
    use wire::event::Payload;

    fn event(payload: Payload) -> wire::Event {
        wire::Event {
            payload: Some(payload),
            ..Default::default()
        }
    }

    #[test]
    fn initialize_echoes_supported_version() {
        let request = InitializeRequest::new(ProtocolVersion::V1);
        let response = initialize_response(&request);
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        assert!(!response.agent_capabilities.load_session);
        assert_eq!(
            response.agent_info.as_ref().map(|info| info.name.as_str()),
            Some("parrot")
        );
    }

    #[test]
    fn prompt_text_joins_text_and_references() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&prompt).as_deref(), Some("first\nsecond"));
        assert_eq!(prompt_text(&[]), None);
    }

    #[test]
    fn empty_prompt_reports_invalid_params() {
        // The handler turns None into an invalid-params error; the mapping
        // itself must not fabricate a prompt.
        assert_eq!(
            prompt_text(&[ContentBlock::Image(ImageContent::new("", "image/png"))]),
            None
        );
    }

    #[test]
    fn stop_reasons_follow_the_documented_mapping() {
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("interrupted"), StopReason::Cancelled);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("incomplete"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("content_filter"), StopReason::Refusal);
    }

    #[test]
    fn text_and_reasoning_chunks_map_to_message_updates() {
        let text = map_event(&event(Payload::TextChunk(wire::TextChunk {
            fragment: "hello".into(),
            ..Default::default()
        })));
        let Some(SessionUpdate::AgentMessageChunk(chunk)) = text else {
            panic!("expected an agent message chunk");
        };
        assert_eq!(chunk.content, ContentBlock::Text(TextContent::new("hello")));

        let reasoning = map_event(&event(Payload::ReasoningChunk(wire::ReasoningChunk {
            fragment: "thinking".into(),
            ..Default::default()
        })));
        assert!(matches!(
            reasoning,
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
    }

    #[test]
    fn tool_events_map_to_the_documented_lifecycle() {
        let started = map_event(&event(Payload::ToolStarted(wire::ToolStarted {
            tool_call_id: "call-1".into(),
            tool_name: "read_file".into(),
            path_variables: [("path".into(), "/tmp/notes.md".into())]
                .into_iter()
                .collect(),
            ..Default::default()
        })));
        let Some(SessionUpdate::ToolCall(call)) = started else {
            panic!("expected a tool call");
        };
        assert_eq!(call.tool_call_id.0.as_ref(), "call-1");
        assert_eq!(call.title, "read_file /tmp/notes.md");
        assert_eq!(call.status, ToolCallStatus::InProgress);
        assert_eq!(call.kind, ToolKind::Read);

        let finished = map_event(&event(Payload::ToolFinished(wire::ToolFinished {
            tool_call_id: "call-1".into(),
            tool_name: "read_file".into(),
            result: Some("file body".into()),
            ..Default::default()
        })));
        let Some(SessionUpdate::ToolCallUpdate(update)) = finished else {
            panic!("expected a tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        let content = update
            .fields
            .content
            .expect("finished tools report content");
        assert_eq!(content.len(), 1);

        let errored = map_event(&event(Payload::ToolError(wire::ToolError {
            tool_call_id: "call-2".into(),
            tool_name: "write_file".into(),
            message: "disk full".into(),
            ..Default::default()
        })));
        let Some(SessionUpdate::ToolCallUpdate(update)) = errored else {
            panic!("expected a tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));

        let cancelled = map_event(&event(Payload::ToolCancelled(wire::ToolCancelled {
            tool_call_id: "call-3".into(),
            tool_name: "read_file".into(),
            ..Default::default()
        })));
        let Some(SessionUpdate::ToolCallUpdate(update)) = cancelled else {
            panic!("expected a tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn lifecycle_events_without_updates_map_to_none() {
        assert_eq!(
            map_event(&event(Payload::TurnStarted(Default::default()))),
            None
        );
        assert_eq!(
            map_event(&event(Payload::InputAdmitted(Default::default()))),
            None
        );
        assert_eq!(
            map_event(&event(Payload::RetryNotice(Default::default()))),
            None
        );
    }

    #[test]
    fn permission_outcomes_map_to_parrot_choices() {
        assert_eq!(permission_choice("allow_once"), "grant");
        assert_eq!(permission_choice("allow_always"), "grant");
        assert_eq!(permission_choice("reject_once"), "reject");
        assert_eq!(permission_choice("reject_always"), "reject");
        assert_eq!(permission_choice("anything_else"), "reject");
    }
}
