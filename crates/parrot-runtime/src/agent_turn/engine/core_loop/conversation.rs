use super::*;
use parrot_core::{
    ConversationAddition, ConversationEvent, ConversationOrigin as CoreOrigin,
    ConversationPersistence, ConversationReceipt, Message,
};

pub(super) fn render_event(mut event: Event, content: ConversationEvent) -> Event {
    match content {
        ConversationEvent::Message => {}
        ConversationEvent::Notice { kind, notice } => {
            event.set_agent_notice(protocol::AgentNotice {
                kind,
                notice,
                ..Default::default()
            })
        }
        ConversationEvent::InputPromoted {
            input_id,
            message_id,
        } => {
            event.set_input_promoted(protocol::InputPromoted {
                input_id,
                message_id,
                ..Default::default()
            });
        }
        ConversationEvent::Retry { reason, attempt } => {
            event.set_retry_notice(protocol::RetryNotice {
                reason,
                attempt,
                ..Default::default()
            })
        }
        ConversationEvent::ToolRequestReceived { count } => {
            event.set_tool_request_received(protocol::ToolRequestReceived {
                tool_call_count: count,
                ..Default::default()
            });
        }
        ConversationEvent::SkillLoaded { path } => {
            event.set_skill_loaded(protocol::SkillLoadedEvent {
                path,
                ..Default::default()
            })
        }
    }
    event
}

fn render_role(role: parrot_core::Role) -> MessageRole {
    match role {
        parrot_core::Role::System => MessageRole::System,
        parrot_core::Role::User => MessageRole::User,
        parrot_core::Role::Assistant => MessageRole::Assistant,
        parrot_core::Role::Tool => MessageRole::Tool,
    }
}

fn render_origin(origin: CoreOrigin) -> ConversationOrigin {
    match origin {
        CoreOrigin::UserInput => ConversationOrigin::UserInput,
        CoreOrigin::Model => ConversationOrigin::Model,
        CoreOrigin::System => ConversationOrigin::System,
        CoreOrigin::Tool => ConversationOrigin::Tool,
    }
}

fn render_record(
    message: &Message,
    origin: ConversationOrigin,
) -> Result<ConversationMessage, TurnError> {
    Ok(ConversationMessage {
        origin,
        role: render_role(message.role),
        parts: message
            .contents
            .iter()
            .map(|content| match content {
                parrot_core::Content::Text(text) => Ok(ConversationPart::text(text)),
                parrot_core::Content::Artifact(image) => Ok(ConversationPart::image(
                    parrot_storage::artifacts::ImageArtifactMetadata {
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
                    },
                )),
                parrot_core::Content::Image { .. } => Err(TurnError::Service(
                    "file images must be admitted as input before they are recorded".into(),
                )),
            })
            .collect::<Result<_, _>>()?,
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| parrot_storage::events::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect(),
        tool_call_id: message.tool_call_id.clone(),
        provider_content: ProviderContent {
            provider_id: message.provider_id.clone(),
            content: message.provider_content.clone(),
        },
    })
}

pub(super) async fn add_conversation<S, T>(
    states: &mut RuntimeStates<'_, S, T>,
    addition: ConversationAddition,
) -> Result<ConversationReceipt, TurnError>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    let backend = states.backend;
    let mut guard = backend.state.lock().await;
    let state = &mut *guard;
    let mut sequence = 0;
    let mut publications = Vec::new();
    let cancellation = states.cancellation;
    match addition.persistence {
        ConversationPersistence::Append { origin, event } => {
            for message in &addition.messages {
                let event = render_event(backend.event(), event.clone());
                let item = backend
                    .history
                    .append_conversation(
                        event.clone(),
                        render_record(message, render_origin(origin))?,
                    )
                    .await?;
                sequence = item.sequence;
                publications.push(event);
            }
        }
        ConversationPersistence::Committed(publication) => {
            let event = render_event(
                Event {
                    id: publication.event_id,
                    agent_session_id: publication.agent_session_id,
                    ..Event::new()
                },
                publication.event,
            );
            publications.push(event);
        }
        persistence @ (ConversationPersistence::Phase { .. }
        | ConversationPersistence::Reminder { .. }
        | ConversationPersistence::Status { .. }) => {
            return crate::compaction::add_conversation(
                &backend.history,
                &mut state.conversation,
                &mut state.context.initial_status_pending,
                ConversationAddition {
                    messages: addition.messages,
                    persistence,
                },
                &ContextEmitter { backend },
                cancellation,
            )
            .await
            .map_err(Into::into);
        }
        persistence @ (ConversationPersistence::ToolResult { .. }
        | ConversationPersistence::ToolImages { .. }) => {
            return state
                .tool_conversation
                .add_conversation(
                    &mut state.conversation,
                    ConversationAddition {
                        messages: addition.messages,
                        persistence,
                    },
                )
                .await;
        }
        ConversationPersistence::CompletionFeedback(feedback) => {
            let mut candidate = Some(feedback.candidate);
            let sources = std::mem::take(
                &mut *states
                    .binding
                    .exit_blocker_sources
                    .lock()
                    .expect("completion sources poisoned"),
            );
            for ((source, kind), message) in sources.into_iter().zip(feedback.messages) {
                source
                    .record_completion_feedback(
                        &CompletionFeedback {
                            kind,
                            message,
                            candidate: candidate.take(),
                        },
                        cancellation,
                    )
                    .await?;
            }
        }
        ConversationPersistence::CloseTurn { finish_reason } => {
            states
                .work
                .join_mutations()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let key = states
                .persistence_key
                .as_ref()
                .ok_or_else(|| TurnError::Service("turn start is unavailable".into()))?;
            let totals = backend.usage.capture().own.totals;
            let mut event = backend.event();
            let completed: sessions::Completion = ProviderResponse {
                finish_reason,
                ..Default::default()
            }
            .into();
            event.set_turn_ended(protocol::TurnEnded {
                finish_reason: completed.finish_reason,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                ..Default::default()
            });
            let record = render_record(&addition.messages[0], ConversationOrigin::Model)?;
            let mut live = Vec::new();
            for message in addition.messages {
                live.push(crate::compaction::materialize_message(&backend.history, message).await?);
            }
            let result = backend
                .history
                .repository()
                .events()
                .close_turn_and_publish(
                    key.clone(),
                    event,
                    Some(record),
                    &PublisherRef(backend.publisher.as_ref()),
                    cancellation,
                    |receipt| {
                        if receipt.closure == TurnClosure::NewlyClosed {
                            for message in live {
                                state.conversation.add(message);
                            }
                        }
                    },
                )
                .await;
            let committed = match &result {
                Ok(receipt) => Some(receipt),
                Err(CloseTurnError::Publication { committed, .. }) => Some(committed.as_ref()),
                _ => None,
            };
            if let Some(receipt) = committed {
                backend.history.refresh().await?;
                if receipt.closure == TurnClosure::AlreadyClosed {
                    state.conversation.reload().await?;
                }
            }
            let receipt = result.map_err(|error| match error {
                CloseTurnError::AdmissionCancelled => TurnError::Cancelled,
                CloseTurnError::Repository(error) => error.into(),
                CloseTurnError::Publication { error, .. } => TurnError::Publication(error),
            })?;
            return Ok(ConversationReceipt {
                inserted: receipt.closure == TurnClosure::NewlyClosed,
                sequence: 0,
            });
        }
    }
    for message in addition.messages {
        if message.role == parrot_core::Role::Assistant {
            let text: String = message
                .contents
                .iter()
                .filter_map(|content| match content {
                    parrot_core::Content::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            backend.activity.assistant_message(&text);
        }
        state
            .conversation
            .add(crate::compaction::materialize_message(&backend.history, message).await?);
    }
    for event in publications {
        backend.publish(event, cancellation).await?;
    }
    Ok(ConversationReceipt {
        inserted: true,
        sequence,
    })
}
