use super::{engine::Publisher, services::TurnError};
use parrot_core::{ToolExecutionRequest, ToolResult, ToolStatus};
use parrot_domain::{
    contracts::storage::Committed,
    protocol::messages::parrot::{
        ArtifactReference, Event, ToolCancelled, ToolError, ToolFinished, YieldedShellProcess,
    },
};
use parrot_storage::{
    conversation::{AgentConversation, AgentHistoryView},
    events::{
        ConversationPart, MessageRole, NeverCancelled, ToolCall, ToolExecutionStatus,
        ToolExecutionTerminal,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

pub struct ToolConversation {
    history: AgentHistoryView,
    publisher: Arc<Publisher>,
    event_id: Arc<dyn Fn() -> String + Send + Sync>,
    watermark: i64,
    scan: Option<ToolScan>,
}

struct ToolScan {
    through: i64,
    batches: VecDeque<ToolBatch>,
}

struct ToolBatch {
    sequence: i64,
    calls: Vec<ToolCall>,
    terminals: HashMap<String, ToolExecutionTerminal>,
    index: usize,
    stopped: bool,
    starts_execution: bool,
}

impl ToolConversation {
    pub fn new(
        history: AgentHistoryView,
        publisher: Arc<Publisher>,
        event_id: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            history,
            publisher,
            event_id,
            watermark: 0,
            scan: None,
        }
    }

    pub fn settled_conversation_sequence(&self) -> i64 {
        self.watermark
    }

    pub fn reset_pending(&mut self) {
        self.scan = None;
    }

    pub async fn prepare_execution(
        &mut self,
        conversation: &mut AgentConversation,
    ) -> Result<Option<ToolExecutionRequest>, TurnError> {
        let mut scan = match self.scan.take() {
            Some(scan) => scan,
            None => self.load_scan().await?,
        };
        while let Some(batch) = scan.batches.front_mut() {
            while let Some(call) = batch.calls.get(batch.index) {
                let Some(terminal) = batch.terminals.get(&call.id).cloned() else {
                    break;
                };
                batch.stopped |= terminal.status == ToolExecutionStatus::Cancelled;
                self.apply_terminal(conversation, batch.sequence, self.event(), terminal)
                    .await?;
                batch.index += 1;
            }
            if batch.index == batch.calls.len() {
                self.finish_batch(conversation, batch).await?;
                scan.batches.pop_front();
                continue;
            }
            let calls = batch.calls[batch.index..]
                .iter()
                .take_while(|call| !batch.terminals.contains_key(&call.id))
                .map(|call| parrot_core::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments_json: call.arguments_json.clone(),
                })
                .collect();
            let mut prior_image_bytes = Vec::new();
            let mut image_budget_exceeded = false;
            if batch.starts_execution {
                for terminal in batch
                    .calls
                    .iter()
                    .filter_map(|call| batch.terminals.get(&call.id))
                {
                    image_budget_exceeded |=
                        terminal.status == ToolExecutionStatus::ImageBudgetExceeded;
                    for part in &terminal.result_parts {
                        if let ConversationPart::ImageArtifact { artifact_id, .. } = part {
                            let image = self
                                .history
                                .resolve_image_artifact(artifact_id.clone())
                                .await?
                                .ok_or_else(|| {
                                    TurnError::Service(
                                        "A settled tool image artifact is missing.".into(),
                                    )
                                })?;
                            prior_image_bytes.push(image.byte_length);
                        }
                    }
                }
            }
            let request = ToolExecutionRequest {
                assistant_sequence: batch.sequence,
                starts_batch: batch.starts_execution,
                calls,
                prior_image_bytes,
                image_budget_exceeded,
                cancel_remaining: batch.stopped,
            };
            batch.starts_execution = false;
            self.scan = Some(scan);
            return Ok(Some(request));
        }
        self.watermark = scan.through;
        Ok(None)
    }

    pub async fn record_results(
        &mut self,
        conversation: &mut AgentConversation,
        results: &[ToolResult],
    ) -> Result<(), TurnError> {
        let mut scan = self
            .scan
            .take()
            .ok_or_else(|| TurnError::Service("tool execution is not prepared".into()))?;
        let batch = scan
            .batches
            .front_mut()
            .ok_or_else(|| TurnError::Service("tool execution batch is unavailable".into()))?;
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
            self.apply_terminal(
                conversation,
                batch.sequence,
                event.clone(),
                terminal.clone(),
            )
            .await?;
            self.publish(event).await?;
            batch.stopped |= terminal.status == ToolExecutionStatus::Cancelled;
            batch
                .terminals
                .insert(terminal.tool_call_id.clone(), terminal);
            batch.index += 1;
        }
        self.scan = Some(scan);
        Ok(())
    }

    async fn load_scan(&self) -> Result<ToolScan, TurnError> {
        let conversation = self.history.conversation_after(self.watermark).await?;
        let through = conversation
            .last()
            .map_or(self.watermark, |item| item.sequence);
        let assistants: Vec<_> = conversation
            .into_iter()
            .filter(|item| {
                item.message.role == MessageRole::Assistant && !item.message.tool_calls.is_empty()
            })
            .collect();
        let terminals: HashMap<_, _> = self
            .history
            .tool_terminals_of_calls(
                assistants
                    .iter()
                    .flat_map(|batch| batch.message.tool_calls.iter().map(|call| call.id.clone()))
                    .collect(),
            )
            .await?
            .into_iter()
            .map(|terminal| (terminal.tool_call_id.clone(), terminal))
            .collect();
        let batches = assistants
            .into_iter()
            .map(|assistant| {
                let completed = assistant
                    .message
                    .tool_calls
                    .iter()
                    .filter_map(|call| {
                        terminals
                            .get(&call.id)
                            .cloned()
                            .map(|terminal| (call.id.clone(), terminal))
                    })
                    .collect();
                ToolBatch {
                    sequence: assistant.sequence,
                    calls: assistant.message.tool_calls,
                    terminals: completed,
                    index: 0,
                    stopped: false,
                    starts_execution: true,
                }
            })
            .collect();
        Ok(ToolScan { through, batches })
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
