use super::{engine::Publisher, services::TurnError};
use parrot_core::{
    BoxFuture, ConfigKey, ConfigValue, Content, ConversationAddition, ConversationPersistence,
    ConversationReceipt, Error, PromptTemplate, States, ToolCall, ToolImage, ToolRecovery,
    ToolRecoveryBatch, ToolResult, ToolSettlement, ToolStatus,
};
use parrot_domain::{
    contracts::storage::Committed,
    protocol::messages::parrot::{
        ArtifactReference, Event, ToolCancelled, ToolError, ToolFinished, YieldedShellProcess,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_storage::{
    artifacts::ImageArtifactMetadata,
    conversation::{AgentConversation, AgentHistoryView},
    events::{
        ConversationItem, ConversationPart, MessageRole, NeverCancelled, ToolExecutionStatus,
        ToolExecutionTerminal,
    },
};
use std::{collections::HashMap, sync::Arc};

pub struct ToolConversation {
    settlement: ToolSettlement,
    backing: ToolBacking,
}

struct ToolBacking {
    history: AgentHistoryView,
    publisher: Arc<Publisher>,
    event_id: Arc<dyn Fn() -> String + Send + Sync>,
    crash_template: Arc<PromptTemplate>,
    error_template: Arc<PromptTemplate>,
    watermark: i64,
}

struct ToolState<'a> {
    backing: &'a mut ToolBacking,
    conversation: &'a mut AgentConversation,
}

struct ToolCommit {
    pub item: Option<ConversationItem>,
    pub events: Vec<Event>,
}

impl ToolConversation {
    pub fn new(
        history: AgentHistoryView,
        publisher: Arc<Publisher>,
        event_id: Arc<dyn Fn() -> String + Send + Sync>,
        templates: &PromptTemplateCatalog,
    ) -> Result<Self, TurnError> {
        Ok(Self {
            settlement: ToolSettlement::default(),
            backing: ToolBacking {
                history,
                publisher,
                event_id,
                crash_template: templates
                    .get_template("agent-session.crashed-tool-result")
                    .map_err(|error| TurnError::Service(error.to_string()))?,
                error_template: templates
                    .get_template("tool-result.error")
                    .map_err(|error| TurnError::Service(error.to_string()))?,
                watermark: 0,
            },
        })
    }

    pub fn settled_conversation_sequence(&self) -> i64 {
        self.backing.watermark
    }

    pub fn advance_watermark(&mut self, through: i64) {
        self.backing.watermark = self.backing.watermark.max(through);
    }

    pub fn reset_pending(&mut self) {
        self.settlement.reset_pending();
    }

    pub async fn restore_results(
        &mut self,
        conversation: &mut AgentConversation,
    ) -> Result<(), TurnError> {
        self.settlement
            .restore_results(&mut ToolState {
                backing: &mut self.backing,
                conversation,
            })
            .await
            .map_err(TurnError::Core)
    }

    pub fn begin_execution(
        &mut self,
        sequence: i64,
        calls: Vec<ToolCall>,
    ) -> Result<(), TurnError> {
        self.settlement
            .begin_execution(sequence, calls)
            .map_err(TurnError::Core)
    }

    pub async fn record_results(
        &mut self,
        conversation: &mut AgentConversation,
        results: &[ToolResult],
    ) -> Result<(), TurnError> {
        self.settlement
            .record_results(
                &mut ToolState {
                    backing: &mut self.backing,
                    conversation,
                },
                results,
            )
            .await
            .map_err(TurnError::Core)
    }

    pub async fn complete_execution(
        &mut self,
        conversation: &mut AgentConversation,
        results: &[ToolResult],
    ) -> Result<(), TurnError> {
        self.settlement
            .complete_execution(
                &mut ToolState {
                    backing: &mut self.backing,
                    conversation,
                },
                results,
            )
            .await
            .map_err(TurnError::Core)
    }

    pub async fn read_recovery(&self) -> Result<ToolRecovery, TurnError> {
        self.backing.read_recovery().await
    }

    pub async fn add_conversation(
        &mut self,
        conversation: &mut AgentConversation,
        addition: ConversationAddition,
    ) -> Result<ConversationReceipt, TurnError> {
        ToolState {
            backing: &mut self.backing,
            conversation,
        }
        .add_conversation(addition)
        .await
        .map_err(TurnError::Core)
    }
}

impl States for ToolState<'_> {
    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async move {
            let template = match key {
                ConfigKey::CrashedToolTemplate => &self.backing.crash_template,
                ConfigKey::ToolErrorTemplate => &self.backing.error_template,
                _ => return Err(Error::unsupported("tool settlement configuration")),
            };
            Ok(ConfigValue::PromptTemplate(template.clone()))
        })
    }

    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<ToolRecovery, Error>> {
        Box::pin(async move { self.backing.read_recovery().await.map_err(Into::into) })
    }

    fn write_tool_watermark(&mut self, through: i64) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.backing.watermark = self.backing.watermark.max(through);
            Ok(())
        })
    }

    fn add_conversation(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            let committed = self
                .backing
                .commit_addition(&addition)
                .await
                .map_err(Error::from)?;
            let receipt = ConversationReceipt {
                inserted: committed.item.is_some(),
                sequence: committed.item.as_ref().map_or(0, |item| item.sequence),
            };
            if let Some(item) = committed.item {
                let message = self
                    .conversation
                    .materialize(item)
                    .await
                    .map_err(TurnError::from)
                    .map_err(Error::from)?;
                self.conversation.add(message);
            }
            for event in committed.events {
                self.backing
                    .publisher
                    .publish_committed(&Committed::after_commit(event), &NeverCancelled)
                    .await
                    .map_err(TurnError::Publication)
                    .map_err(Error::from)?;
            }
            Ok(receipt)
        })
    }
}

impl ToolBacking {
    async fn read_recovery(&self) -> Result<ToolRecovery, TurnError> {
        let messages = self.history.conversation_after(self.watermark).await?;
        let through = messages.last().map_or(self.watermark, |item| item.sequence);
        let assistants = messages
            .into_iter()
            .filter(|item| {
                item.message.role == MessageRole::Assistant && !item.message.tool_calls.is_empty()
            })
            .collect::<Vec<_>>();
        let terminals = self
            .history
            .tool_terminals_of_calls(
                assistants
                    .iter()
                    .flat_map(|assistant| {
                        assistant
                            .message
                            .tool_calls
                            .iter()
                            .map(|call| call.id.clone())
                    })
                    .collect(),
            )
            .await?
            .into_iter()
            .map(|terminal| (terminal.tool_call_id.clone(), terminal))
            .collect::<HashMap<_, _>>();
        let mut batches = Vec::new();
        for assistant in assistants {
            let results = assistant
                .message
                .tool_calls
                .iter()
                .filter_map(|call| terminals.get(&call.id).cloned())
                .map(|terminal| {
                    let text = parrot_storage::events::content(&terminal.result_parts);
                    ToolResult {
                        call_id: terminal.tool_call_id,
                        tool_name: terminal.tool_name,
                        status: match terminal.status {
                            ToolExecutionStatus::Finished => ToolStatus::Finished,
                            ToolExecutionStatus::Cancelled => ToolStatus::Cancelled,
                            ToolExecutionStatus::Error => ToolStatus::Error,
                            ToolExecutionStatus::ImageBudgetExceeded => {
                                ToolStatus::ImageBudgetExceeded
                            }
                        },
                        text,
                        error_message: String::new(),
                        metadata: None,
                        images: terminal
                            .result_parts
                            .into_iter()
                            .filter_map(|part| match part {
                                ConversationPart::ImageArtifact { metadata } => Some(ToolImage {
                                    artifact_id: metadata.artifact_id,
                                    sha256: metadata.sha256,
                                    media_type: metadata.media_type,
                                    byte_length: metadata.byte_length,
                                    width: metadata.width,
                                    height: metadata.height,
                                    frame_count: metadata.frame_count,
                                    aggregate_pixels: metadata.aggregate_pixels,
                                    display_name: metadata.display_name,
                                    origin: metadata.origin,
                                }),
                                ConversationPart::Text(_) => None,
                            })
                            .collect(),
                    }
                })
                .collect();
            batches.push(ToolRecoveryBatch {
                sequence: assistant.sequence,
                calls: assistant
                    .message
                    .tool_calls
                    .into_iter()
                    .map(|call| ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments_json: call.arguments_json,
                    })
                    .collect(),
                results,
            });
        }
        Ok(ToolRecovery { through, batches })
    }

    async fn commit_addition(
        &self,
        addition: &ConversationAddition,
    ) -> Result<ToolCommit, TurnError> {
        match &addition.persistence {
            ConversationPersistence::ToolResult {
                assistant_sequence,
                result,
                recovered,
            } => {
                let terminal = ToolExecutionTerminal {
                    tool_call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    status: match result.status {
                        ToolStatus::Finished => ToolExecutionStatus::Finished,
                        ToolStatus::Cancelled => ToolExecutionStatus::Cancelled,
                        ToolStatus::Error => ToolExecutionStatus::Error,
                        ToolStatus::ImageBudgetExceeded => ToolExecutionStatus::ImageBudgetExceeded,
                    },
                    result_parts: addition
                        .messages
                        .iter()
                        .flat_map(|message| message.contents.iter())
                        .map(|content| match content {
                            Content::Text(text) => Ok(ConversationPart::text(text)),
                            Content::Artifact(image) => {
                                Ok(ConversationPart::image(ImageArtifactMetadata {
                                    artifact_id: image.artifact_id.clone(),
                                    sha256: image.sha256.clone(),
                                    media_type: image.media_type.clone(),
                                    byte_length: image.byte_length,
                                    width: image.width,
                                    height: image.height,
                                    frame_count: image.frame_count,
                                    aggregate_pixels: image.aggregate_pixels,
                                    display_name: image.display_name.clone(),
                                    origin: image.origin.clone(),
                                }))
                            }
                            Content::Image { .. } => Err(TurnError::Service(
                                "tool settlement requires artifact-backed images".into(),
                            )),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                };
                let mut event = self.event();
                if !recovered {
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
                                .metadata
                                .as_deref()
                                .map(<YieldedShellProcess as protobuf::Message>::parse_from_bytes)
                                .transpose()
                                .map_err(|error| TurnError::Service(error.to_string()))?
                                .into(),
                            ..ToolFinished::new()
                        }),
                        ToolStatus::Cancelled => event.set_tool_cancelled(ToolCancelled {
                            tool_call_id: result.call_id.clone(),
                            tool_name: result.tool_name.clone(),
                            ..ToolCancelled::new()
                        }),
                        ToolStatus::Error | ToolStatus::ImageBudgetExceeded => event
                            .set_tool_error(ToolError {
                                tool_call_id: result.call_id.clone(),
                                tool_name: result.tool_name.clone(),
                                message: result.error_message.clone(),
                                ..ToolError::new()
                            }),
                    }
                }
                let item = self
                    .history
                    .append_tool_settlement(event.clone(), *assistant_sequence, terminal)
                    .await?;
                Ok(ToolCommit {
                    item,
                    events: if *recovered { Vec::new() } else { vec![event] },
                })
            }
            ConversationPersistence::ToolImages { assistant_sequence } => {
                let parts = addition
                    .messages
                    .iter()
                    .flat_map(|message| message.contents.iter())
                    .map(|content| match content {
                        Content::Artifact(image) => {
                            Ok(ConversationPart::image(ImageArtifactMetadata {
                                artifact_id: image.artifact_id.clone(),
                                sha256: image.sha256.clone(),
                                media_type: image.media_type.clone(),
                                byte_length: image.byte_length,
                                width: image.width,
                                height: image.height,
                                frame_count: image.frame_count,
                                aggregate_pixels: image.aggregate_pixels,
                                display_name: image.display_name.clone(),
                                origin: image.origin.clone(),
                            }))
                        }
                        _ => Err(TurnError::Service(
                            "synthetic tool images require artifact content".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let event = self.event();
                let item = self
                    .history
                    .append_tool_synthetic(event.clone(), *assistant_sequence, parts)
                    .await?;
                let events = if item.is_some() {
                    vec![event]
                } else {
                    Vec::new()
                };
                Ok(ToolCommit { item, events })
            }
            _ => Err(TurnError::Service(
                "tool backing received a non-tool conversation addition".into(),
            )),
        }
    }

    fn event(&self) -> Event {
        Event {
            id: (self.event_id)(),
            agent_session_id: self.history.agent_session_id().into(),
            ..Event::new()
        }
    }
}
