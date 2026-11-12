//! Turn engine: drains ready turns and drives one turn from input to settlement.

use crate::{
    AgentState, BoxFuture, CancellationToken, CompletionBlockerFeedback, ConfigKey,
    ConversationAddition, ConversationEvent, ConversationOrigin, ConversationPersistence,
    ConversationTurn, Error, ErrorKind, EventPublisher, FinishReason, InputAdmission, Message,
    Outcome, Provider, Role, States, ToolCallReceipt, ToolResultMetadata, Tools, add_phase_prompt,
    context, execute_tools, rendering, skills, steer_status,
};

/// Supplies the drain loop with runtime-owned lifecycle facts and per-turn work.
pub trait TurnLifecycle: Send {
    /// Reports whether runtime-owned work is scheduled and must run before another turn.
    fn has_scheduled_work(&self) -> bool;
    /// Reports whether a turn awaits a response or pending input remains.
    fn has_ready_turn(&mut self) -> BoxFuture<'_, Result<bool, Error>>;
    /// Runs one turn, including its open and finish effects.
    fn run_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Outcome, Error>>;
}

/// Drains ready turns using state-backed settings and the invocation budget.
pub async fn run(
    lifecycle: &mut dyn TurnLifecycle,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut answer = String::new();
    loop {
        if lifecycle.has_scheduled_work() {
            return Ok(Outcome::Completed(answer));
        }
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let token = CancellationToken::new(&cancellation);
        if !lifecycle.has_ready_turn().await? {
            return Ok(Outcome::Completed(answer));
        }
        match lifecycle.run_turn(token).await? {
            Outcome::Completed(completed) => answer = completed,
            exhausted @ Outcome::BudgetExhausted(_) => return Ok(exhausted),
        }
    }
}

/// Opens the turn through the states and publisher sides. The states side begins the
/// turn and names the model; the publisher renders and persists the start event; the
/// states side records that event as the turn's durable key and activates the turn.
/// Callers run exactly one `run_turn` after each open.
pub async fn open_turn(
    states: &mut dyn States,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
) -> Result<(), Error> {
    let model = states.begin_turn().await?;
    let turn_id = publisher.report_turn_started(&model, cancellation).await?;
    states.record_turn_started(&turn_id).await?;
    states.activate_turn().await
}

/// Runs one active turn using state-backed settings and tools captured once.
/// The caller opens the turn first.
pub async fn run_turn<M: ToolResultMetadata>(
    states: &mut dyn States,
    tools: &dyn Tools<M>,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    let mut state = states.get_agent_state().await?;
    let result = execute_turn(states, tools, provider, publisher, cancellation, &mut state).await;
    states.set_agent_state(state).await?;
    result
}

async fn execute_turn<M: ToolResultMetadata>(
    states: &mut dyn States,
    tools: &dyn Tools<M>,
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
                .add_conversation_messages(ConversationAddition {
                    messages: vec![input.message.clone()],
                    persistence: ConversationPersistence::Committed(input.publication.clone()),
                })
                .await?;
        }
        let context = skills.prepare(states, &inputs, cancellation).await?;
        if !context.content.is_empty() {
            states
                .add_conversation_messages(ConversationAddition {
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
            return exhaust_budget(states).await;
        };
        let definitions = if flags.final_phase {
            &catalog.final_phase
        } else {
            &catalog.normal
        };
        steer_status(states, provider, definitions, cancellation).await?;
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
                .add_conversation_messages(ConversationAddition {
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
                .add_conversation_messages(ConversationAddition {
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
                .add_conversation_messages(ConversationAddition {
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
                return exhaust_budget(states).await;
            }
            continue;
        }
        let messages = states.collect_completion_blockers().await?;
        if messages.is_empty() {
            let usage = states.read_usage_totals().await?;
            states
                .add_conversation_messages(ConversationAddition {
                    messages: vec![Message::from_response(&response)],
                    persistence: ConversationPersistence::CloseTurn {
                        finish_reason: response.finish_reason.clone(),
                        usage,
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
            .add_conversation_messages(ConversationAddition {
                messages: additions,
                persistence: ConversationPersistence::CompletionBlockerFeedback(
                    CompletionBlockerFeedback {
                        messages,
                        candidate: response,
                    },
                ),
            })
            .await?;
        state.restart();
    }
}

/// Renders the terminal notice core records when the invocation budget runs out.
async fn exhaust_budget(states: &dyn States) -> Result<Outcome, Error> {
    let notice = rendering::render_template(states, ConfigKey::RunawayTemplate, &[]).await?;
    Ok(Outcome::BudgetExhausted(notice))
}

/// Records an interrupted assistant turn through the same generic conversation operation.
pub async fn interrupt_turn(states: &mut dyn States) -> Result<Option<String>, Error> {
    let content = rendering::render_template(states, ConfigKey::InterruptedTemplate, &[]).await?;
    let usage = states.read_usage_totals().await?;
    let receipt = states
        .add_conversation_messages(ConversationAddition {
            messages: vec![Message::text(Role::Assistant, &content)],
            persistence: ConversationPersistence::CloseTurn {
                finish_reason: FinishReason::Interrupted,
                usage,
            },
        })
        .await?;
    Ok(receipt.inserted.then_some(content))
}
