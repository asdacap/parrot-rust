use super::{engine::Publisher, services::TurnError};
use parrot_core::{ToolCall, ToolResult, ToolStatus};
use parrot_domain::{
    contracts::storage::Committed,
    protocol::messages::parrot::{
        ArtifactReference, Event, ToolCancelled, ToolError, ToolFinished, YieldedShellProcess,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::{
    conversation::{AgentConversation, AgentHistoryView},
    events::{
        ConversationPart, MessageRole, NeverCancelled, ToolExecutionStatus, ToolExecutionTerminal,
    },
};
use std::{collections::HashMap, sync::Arc};

pub struct ToolConversation {
    history: AgentHistoryView,
    publisher: Arc<Publisher>,
    event_id: Arc<dyn Fn() -> String + Send + Sync>,
    crash_message: String,
    crash_result: String,
    watermark: i64,
    pending: Option<ToolBatch>,
}

struct ToolBatch {
    sequence: i64,
    calls: Vec<ToolCall>,
    terminals: HashMap<String, ToolExecutionTerminal>,
    index: usize,
}

impl ToolConversation {
    pub fn new(
        history: AgentHistoryView,
        publisher: Arc<Publisher>,
        event_id: Arc<dyn Fn() -> String + Send + Sync>,
        templates: &PromptTemplateCatalog,
    ) -> Result<Self, TurnError> {
        let crash_message = templates
            .render("agent-session.crashed-tool-result", &[], &NeverCancelled)
            .map_err(|error| TurnError::Service(error.to_string()))?;
        let crash_result = templates
            .render(
                "tool-result.error",
                &[(
                    "message".into(),
                    serde_json::Value::String(crash_message.clone()),
                )],
                &NeverCancelled,
            )
            .map_err(|error| TurnError::Service(error.to_string()))?;
        Ok(Self {
            history,
            publisher,
            event_id,
            crash_message,
            crash_result,
            watermark: 0,
            pending: None,
        })
    }

    pub fn settled_conversation_sequence(&self) -> i64 {
        self.watermark
    }

    pub fn reset_pending(&mut self) {
        self.pending = None;
    }

    pub async fn restore_results(
        &mut self,
        conversation: &mut AgentConversation,
    ) -> Result<(), TurnError> {
        if self.pending.is_some() {
            return Err(TurnError::Service(
                "cannot restore results during tool execution".into(),
            ));
        }
        let messages = self.history.conversation_after(self.watermark).await?;
        let through = messages.last().map_or(self.watermark, |item| item.sequence);
        let assistants: Vec<_> = messages
            .into_iter()
            .filter(|item| {
                item.message.role == MessageRole::Assistant && !item.message.tool_calls.is_empty()
            })
            .collect();
        let mut terminals: HashMap<_, _> = self
            .history
            .tool_terminals_of_calls(
                assistants
                    .iter()
                    .flat_map(|item| item.message.tool_calls.iter().map(|call| call.id.clone()))
                    .collect(),
            )
            .await?
            .into_iter()
            .map(|terminal| (terminal.tool_call_id.clone(), terminal))
            .collect();
        for assistant in assistants {
            let calls: Vec<_> = assistant
                .message
                .tool_calls
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments_json: call.arguments_json,
                })
                .collect();
            let mut completed = HashMap::new();
            for call in &calls {
                let terminal = if let Some(terminal) = terminals.get(&call.id).cloned() {
                    self.apply_terminal(
                        conversation,
                        assistant.sequence,
                        self.event(),
                        terminal.clone(),
                    )
                    .await?;
                    terminal
                } else {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        status: ToolStatus::Error,
                        text: self.crash_result.clone(),
                        error_message: self.crash_message.clone(),
                        images: Vec::new(),
                        yielded_process: None,
                    };
                    let terminal = self
                        .record_result(conversation, assistant.sequence, &result)
                        .await?;
                    terminals.insert(call.id.clone(), terminal.clone());
                    terminal
                };
                completed.insert(call.id.clone(), terminal);
            }
            let batch = ToolBatch {
                sequence: assistant.sequence,
                calls,
                terminals: completed,
                index: 0,
            };
            self.finish_batch(conversation, &batch).await?;
        }
        self.watermark = through;
        Ok(())
    }

    pub fn begin_execution(
        &mut self,
        sequence: i64,
        calls: Vec<ToolCall>,
    ) -> Result<(), TurnError> {
        if self.pending.is_some() {
            return Err(TurnError::Service(
                "the previous tool batch is unfinished".into(),
            ));
        }
        self.pending = Some(ToolBatch {
            sequence,
            calls,
            terminals: HashMap::new(),
            index: 0,
        });
        Ok(())
    }

    pub async fn complete_execution(
        &mut self,
        conversation: &mut AgentConversation,
        results: &[ToolResult],
    ) -> Result<(), TurnError> {
        let batch = self
            .pending
            .take()
            .ok_or_else(|| TurnError::Service("tool execution is not prepared".into()))?;
        if batch.index != batch.calls.len()
            || results.len() != batch.calls.len()
            || results
                .iter()
                .zip(&batch.calls)
                .any(|(result, call)| result.call_id != call.id || result.tool_name != call.name)
        {
            return Err(TurnError::Service(
                "tool execution did not complete the recorded batch".into(),
            ));
        }
        self.finish_batch(conversation, &batch).await?;
        self.watermark = self.watermark.max(batch.sequence);
        Ok(())
    }

    pub async fn record_results(
        &mut self,
        conversation: &mut AgentConversation,
        results: &[ToolResult],
    ) -> Result<(), TurnError> {
        let mut batch = self
            .pending
            .take()
            .ok_or_else(|| TurnError::Service("tool execution is not prepared".into()))?;
        if results.is_empty() {
            return Err(TurnError::Service(
                "tool execution returned no results".into(),
            ));
        }
        for result in results {
            let call = batch.calls.get(batch.index).ok_or_else(|| {
                TurnError::Service("tool execution returned too many results".into())
            })?;
            if result.call_id != call.id || result.tool_name != call.name {
                return Err(TurnError::Service(
                    "tool result does not match the prepared call".into(),
                ));
            }
            let terminal = self
                .record_result(conversation, batch.sequence, result)
                .await?;
            batch
                .terminals
                .insert(terminal.tool_call_id.clone(), terminal);
            batch.index += 1;
        }
        self.pending = Some(batch);
        Ok(())
    }

    async fn record_result(
        &self,
        conversation: &mut AgentConversation,
        sequence: i64,
        result: &ToolResult,
    ) -> Result<ToolExecutionTerminal, TurnError> {
        let terminal = ToolExecutionTerminal {
            tool_call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            status: match result.status {
                ToolStatus::Finished => ToolExecutionStatus::Finished,
                ToolStatus::Cancelled => ToolExecutionStatus::Cancelled,
                ToolStatus::Error => ToolExecutionStatus::Error,
                ToolStatus::ImageBudgetExceeded => ToolExecutionStatus::ImageBudgetExceeded,
            },
            result_parts: std::iter::once(ConversationPart::text(&result.text))
                .chain(result.images.iter().map(|image| {
                    ConversationPart::image(
                        &image.artifact_id,
                        &image.media_type,
                        &image.display_name,
                    )
                }))
                .collect(),
            message: result.text.clone(),
        };
        let mut event = self.event();
        match result.status {
            ToolStatus::Finished => event.set_tool_finished(ToolFinished {
                tool_call_id: result.call_id.clone(),
                tool_name: result.tool_name.clone(),
                result: Some(result.text.clone()),
                artifacts: result
                    .images
                    .iter()
                    .map(|image| ArtifactReference {
                        artifact_id: image.artifact_id.clone(),
                        sha256: image.sha256.clone(),
                        media_type: image.media_type.clone(),
                        byte_length: image.byte_length,
                        width: image.width as i32,
                        height: image.height as i32,
                        frame_count: image.frame_count as i32,
                        aggregate_pixels: image.aggregate_pixels,
                        display_name: image.display_name.clone(),
                        origin: image.origin.clone(),
                        ..ArtifactReference::new()
                    })
                    .collect(),
                yielded_process: result
                    .yielded_process
                    .as_ref()
                    .map(|process| YieldedShellProcess {
                        process_id: process.process_id.clone(),
                        name: process.name.clone(),
                        inventory_instance_id: process.inventory_instance_id.clone(),
                        visible_revision: process.visible_revision,
                        stdout_path: process.stdout_path.clone(),
                        stderr_path: process.stderr_path.clone(),
                        activity_kind: protobuf::EnumOrUnknown::from_i32(process.activity_kind),
                        ..YieldedShellProcess::new()
                    })
                    .into(),
                ..ToolFinished::new()
            }),
            ToolStatus::Cancelled => event.set_tool_cancelled(ToolCancelled {
                tool_call_id: result.call_id.clone(),
                tool_name: result.tool_name.clone(),
                ..ToolCancelled::new()
            }),
            ToolStatus::Error | ToolStatus::ImageBudgetExceeded => {
                event.set_tool_error(ToolError {
                    tool_call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    message: result.error_message.clone(),
                    ..ToolError::new()
                })
            }
        }
        self.apply_terminal(conversation, sequence, event.clone(), terminal.clone())
            .await?;
        self.publish(event).await?;
        Ok(terminal)
    }

    async fn apply_terminal(
        &self,
        conversation: &mut AgentConversation,
        sequence: i64,
        event: Event,
        terminal: ToolExecutionTerminal,
    ) -> Result<(), TurnError> {
        if let Some(item) = self
            .history
            .append_tool_settlement(event, sequence, terminal)
            .await?
        {
            let message = conversation.materialize(item).await?;
            conversation.add(message);
        }
        Ok(())
    }

    async fn finish_batch(
        &self,
        conversation: &mut AgentConversation,
        batch: &ToolBatch,
    ) -> Result<(), TurnError> {
        let images: Vec<_> = batch
            .calls
            .iter()
            .filter_map(|call| batch.terminals.get(&call.id))
            .filter(|terminal| terminal.status == ToolExecutionStatus::Finished)
            .flat_map(|terminal| terminal.result_parts.iter())
            .filter(|part| matches!(part, ConversationPart::ImageArtifact { .. }))
            .cloned()
            .collect();
        if !images.is_empty() && !self.history.has_tool_synthetic(batch.sequence).await? {
            let event = self.event();
            if let Some(item) = self
                .history
                .append_tool_synthetic(event.clone(), batch.sequence, images)
                .await?
            {
                let message = conversation.materialize(item).await?;
                conversation.add(message);
            }
            self.publish(event).await?;
        }
        Ok(())
    }

    fn event(&self) -> Event {
        Event {
            id: (self.event_id)(),
            agent_session_id: self.history.agent_session_id().into(),
            ..Event::new()
        }
    }

    async fn publish(&self, event: Event) -> Result<(), TurnError> {
        self.publisher
            .publish_committed(&Committed::after_commit(event), &NeverCancelled)
            .await
            .map_err(TurnError::Publication)
    }
}
