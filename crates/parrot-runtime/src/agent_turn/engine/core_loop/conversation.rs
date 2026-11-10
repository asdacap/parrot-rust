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

fn render_live(message: Message) -> ModelMessage {
    ModelMessage {
        role: render_role(message.role),
        contents: message
            .contents
            .into_iter()
            .map(|content| match content {
                parrot_core::Content::Text(text) => ModelContent::Text(text),
                parrot_core::Content::Image {
                    path,
                    media_type,
                    width,
                    height,
                    ..
                } => ModelContent::ImageFile {
                    path: path.into(),
                    media_type,
                    width,
                    height,
                },
            })
            .collect(),
        tool_calls: message
            .tool_calls
            .into_iter()
            .map(|call| parrot_storage::events::ToolCall {
                id: call.id,
                name: call.name,
                arguments_json: call.arguments_json,
            })
            .collect(),
        tool_call_id: message.tool_call_id,
        provider_content: ProviderContent {
            provider_id: message.provider_id,
            content: message.provider_content,
        },
    }
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
    let uncancelled_publication = matches!(
        &addition.persistence,
        ConversationPersistence::Reminder { .. }
    );
    let context_publication = matches!(
        &addition.persistence,
        ConversationPersistence::Phase { .. } | ConversationPersistence::Reminder { .. }
    );
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
        ConversationPersistence::Phase { final_phase, event } => {
            let event = render_event(backend.event(), event);
            let content = addition.messages[0]
                .contents
                .iter()
                .filter_map(|part| match part {
                    parrot_core::Content::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let inserted = if final_phase {
                backend
                    .history
                    .append_final_provider_request_prompt(event.clone(), content)
                    .await?
            } else {
                backend
                    .history
                    .append_tool_availability_restored_prompt(event.clone(), content)
                    .await?
            };
            if !inserted {
                return Ok(ConversationReceipt::default());
            }
            publications.push(event);
        }
        ConversationPersistence::Reminder { checkpoint, event } => {
            let event = render_event(backend.event(), event);
            let content = addition.messages[0]
                .contents
                .iter()
                .filter_map(|part| match part {
                    parrot_core::Content::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !backend
                .history
                .append_context_reminder(
                    event.clone(),
                    parrot_storage::conversation::ContextReminderCheckpoint {
                        canonical_model: checkpoint.canonical_model,
                        context_limit: checkpoint.context_limit,
                        percentage: checkpoint.percentage,
                    },
                    content,
                )
                .await?
            {
                return Ok(ConversationReceipt::default());
            }
            publications.push(event);
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
            let live: Vec<_> = addition.messages.into_iter().map(render_live).collect();
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
        state.conversation.add(render_live(message));
    }
    for event in publications {
        let publication_cancellation: &dyn Cancellation = if uncancelled_publication {
            &NeverCancelled
        } else {
            cancellation
        };
        if context_publication {
            ContextEmitter { backend }
                .publish(event, publication_cancellation)
                .await?;
        } else {
            backend.publish(event, publication_cancellation).await?;
        }
    }
    Ok(ConversationReceipt {
        inserted: true,
        sequence,
    })
}
