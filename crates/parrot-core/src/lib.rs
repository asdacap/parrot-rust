//! Turn orchestration with core-owned provider, tool, state, and event contracts.

pub mod budget;
pub mod compaction;
mod context;
mod context_additions;
mod context_epoch;
mod conversation;
mod protocol;
mod rendering;
mod skills;
mod status;
pub mod template;
mod tool_settlement;
pub use context::*;
pub use context_additions::*;
pub use context_epoch::*;
pub use conversation::*;
pub use protocol::*;
pub use skills::{SkillMention, parse_skill_mentions};
pub use status::*;
pub use tool_settlement::*;

/// Drains ready turns using state-backed settings and the invocation budget.
pub async fn run(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut answer = String::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let turn = states.get_last_turn().await?;
        if !turn.as_ref().is_some_and(ConversationTurn::needs_response)
            && states.peek_pending_input_header().await?.is_none()
        {
            return Ok(Outcome::Completed(answer));
        }
        match run_turn(states, tools, provider, publisher, cancellation).await? {
            Outcome::Completed(completed) => answer = completed,
            Outcome::BudgetExhausted => return Ok(Outcome::BudgetExhausted),
        }
    }
}

/// Runs one active turn using state-backed settings and tools captured once.
pub async fn run_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut state = states.get_agent_state().await?;
    let result = execute_turn(states, tools, provider, publisher, cancellation, &mut state).await;
    states.set_agent_state(state).await?;
    result
}

async fn execute_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
    state: &mut AgentState,
) -> Result<Outcome, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let options = states.get_config(ConfigKey::Turn).await?.into_turn()?;
    state.begin_turn();
    let catalog = tools.capture_catalog().await?;
    if state.final_phase {
        let restored =
            rendering::render_template(states, ConfigKey::RestoredToolsTemplate, &[]).await?;
        add_phase_prompt(states, &restored, false).await?;
    }
    let mut compacted_oversized = false;
    let mut skills = skills::SkillPreparation::default();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let turn = states.get_last_turn().await?;
        let admission = if turn.as_ref().is_some_and(ConversationTurn::needs_response) {
            InputAdmission::SteersOnly
        } else {
            InputAdmission::SteersOrNextQueued
        };
        let inputs = states.promote_pending_inputs(admission).await?;
        for input in &inputs {
            states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![input.message.clone()],
                    persistence: ConversationPersistence::Committed(input.publication.clone()),
                })
                .await?;
        }
        let context = skills.prepare(states, &inputs, cancellation).await?;
        if !context.content.is_empty() {
            states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![Message::text(Role::User, context.content)],
                    persistence: ConversationPersistence::Append {
                        origin: ConversationOrigin::System,
                        event: ConversationEvent::Notice {
                            kind: "skill_context".into(),
                            notice: String::new(),
                        },
                    },
                })
                .await?;
            for path in context.loaded_paths {
                publisher
                    .publish(ConversationEvent::SkillLoaded { path }, cancellation)
                    .await?;
            }
        }
        let Some(flags) = state.admit(options.maximum_requests) else {
            return Ok(Outcome::BudgetExhausted);
        };
        let definitions = if flags.final_phase {
            &catalog.final_phase
        } else {
            &catalog.normal
        };
        let request = context::prepare_request(
            states,
            provider,
            publisher,
            definitions,
            flags,
            &options,
            cancellation,
        )
        .await?;
        let attempt = state.count_request();
        let response = match provider.invoke(request, cancellation).await {
            Ok(response) => response,
            Err(error) if error.kind == ErrorKind::RequestTooLarge && !compacted_oversized => {
                compacted_oversized = true;
                if !context::compact_context(states, provider, publisher, definitions, cancellation)
                    .await?
                    .reduced
                {
                    return Err(error);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        compacted_oversized = false;
        if response.finish_reason == FinishReason::Length && !response.tool_calls.is_empty() {
            let content =
                rendering::render_template(states, ConfigKey::TruncatedToolCallTemplate, &[])
                    .await?;
            states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![Message::text(Role::System, &content)],
                    persistence: ConversationPersistence::Append {
                        origin: ConversationOrigin::System,
                        event: ConversationEvent::Retry {
                            reason: content,
                            attempt,
                        },
                    },
                })
                .await?;
            continue;
        }
        if response
            .tool_calls
            .iter()
            .any(|call| serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err())
        {
            let content =
                rendering::render_template(states, ConfigKey::InvalidToolCallTemplate, &[]).await?;
            states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![Message::text(Role::System, &content)],
                    persistence: ConversationPersistence::Append {
                        origin: ConversationOrigin::System,
                        event: ConversationEvent::Retry {
                            reason: content,
                            attempt,
                        },
                    },
                })
                .await?;
            continue;
        }
        if !response.tool_calls.is_empty() {
            let recorded = states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![Message::from_response(&response)],
                    persistence: ConversationPersistence::Append {
                        origin: ConversationOrigin::Model,
                        event: ConversationEvent::Message,
                    },
                })
                .await?;
            publisher
                .publish(
                    ConversationEvent::ToolRequestReceived {
                        count: response.tool_calls.len() as i32,
                    },
                    cancellation,
                )
                .await?;
            let receipt = ToolCallReceipt {
                assistant_sequence: recorded.sequence,
            };
            execute_tools(
                states,
                tools,
                response.tool_calls.clone(),
                receipt,
                flags,
                cancellation,
            )
            .await?;
            if flags.final_phase && !state.try_extend_settlement() {
                return Ok(Outcome::BudgetExhausted);
            }
            continue;
        }
        let messages = states.collect_completion_blockers().await?;
        if messages.is_empty() {
            states
                .apply_conversation_addition(ConversationAddition {
                    messages: vec![Message::from_response(&response)],
                    persistence: ConversationPersistence::CloseTurn {
                        finish_reason: response.finish_reason.clone(),
                    },
                })
                .await?;
            return Ok(Outcome::Completed(response.assistant_text));
        }
        let mut additions = vec![Message::from_response(&response)];
        additions.extend(
            messages
                .iter()
                .map(|message| Message::text(Role::System, message)),
        );
        states
            .apply_conversation_addition(ConversationAddition {
                messages: additions,
                persistence: ConversationPersistence::CompletionFeedback(CompletionFeedback {
                    messages,
                    candidate: response,
                }),
            })
            .await?;
        state.restart();
    }
}

/// Records an interrupted assistant turn through the same generic conversation operation.
pub async fn interrupt_turn(states: &mut dyn States) -> Result<Option<String>, Error> {
    let content = rendering::render_template(states, ConfigKey::InterruptedTemplate, &[]).await?;
    let receipt = states
        .apply_conversation_addition(ConversationAddition {
            messages: vec![Message::text(Role::Assistant, &content)],
            persistence: ConversationPersistence::CloseTurn {
                finish_reason: FinishReason::Interrupted,
            },
        })
        .await?;
    Ok(receipt.inserted.then_some(content))
}
