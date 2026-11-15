//! Turn engine: drains ready turns and drives one turn from input to settlement.

use crate::{
    AgentState, CancellationToken, CompletionBlockerFeedback, ConfigKey, ContextSnapshot,
    ConversationAddition, ConversationEvent, ConversationOrigin, ConversationPersistence,
    ConversationTurn, Error, ErrorKind, EventPublisher, FinishReason, InputAdmission, Message,
    OpenedTurn, Outcome, Provider, Role, SkillProvider, States, ToolCallReceipt, ToolDefinition,
    Tools, TurnEngine, TurnHooks, TurnLifecycle, context, context::compact_context, execute_tools,
    rendering, skills, steer_status,
};

impl OpenedTurn<'_> {
    pub fn start_event_id(&self) -> &str {
        &self.start_event_id
    }
}

struct TurnServices<'a> {
    skills: &'a dyn SkillProvider,
    provider: &'a dyn Provider,
    publisher: &'a dyn EventPublisher,
}

async fn execute_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    services: &TurnServices<'_>,
    opened: &OpenedTurn<'_>,
    cancellation: CancellationToken<'_>,
    state: &mut AgentState,
) -> Result<Outcome, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    state.begin_turn();
    if state.final_phase {
        let restored =
            rendering::render_template(states, ConfigKey::RestoredToolsTemplate, &[]).await?;
        TurnEngine::add_phase_prompt(states, state, &restored, false).await?;
    }
    let mut compacted_oversized = false;
    let mut skills = skills::SkillPreparation::default();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        opened.hooks.begin_step().await?;
        let options = states.get_config(ConfigKey::Turn).await?.into_turn()?;
        let catalog = tools.capture_catalog().await?;
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
        let context = skills
            .prepare(states, services.skills, &inputs, cancellation)
            .await?;
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
                services
                    .publisher
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
        steer_status(states, state, services.provider, definitions, cancellation).await?;
        if flags.final_phase {
            let content =
                rendering::render_template(states, ConfigKey::FinalRequestTemplate, &[]).await?;
            TurnEngine::add_phase_prompt(states, state, &content, true).await?;
        }
        let prepared = context::prepare_request(
            states,
            state,
            services.provider,
            definitions,
            &options,
            cancellation,
        )
        .await?;
        let attempt = state.count_request();
        states.set_agent_state(state.clone()).await?;
        let invoked = services
            .provider
            .invoke(prepared.request, cancellation)
            .await;
        let input_tokens = match &invoked {
            Ok(response) => Some(response.input_tokens),
            Err(error) => error.input_tokens,
        };
        if let Some(input_tokens) = input_tokens {
            state.last_request_statistic.observe_usage(
                &prepared.model_selector,
                prepared.estimated_input_tokens,
                input_tokens,
            );
        }
        let response = match invoked.map_err(|error| error.error) {
            Ok(response) => response,
            Err(error) if error.kind == ErrorKind::RequestTooLarge && !compacted_oversized => {
                compacted_oversized = true;
                if !context::compact_context(
                    states,
                    state,
                    services.provider,
                    definitions,
                    crate::CompactionRequest::default(),
                    cancellation,
                )
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
        let retry_template =
            if response.finish_reason == FinishReason::Length && !response.tool_calls.is_empty() {
                Some(ConfigKey::TruncatedToolCallTemplate)
            } else if response.tool_calls.iter().any(|call| {
                serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err()
            }) {
                Some(ConfigKey::InvalidToolCallTemplate)
            } else {
                None
            };
        if let Some(key) = retry_template {
            let content = rendering::render_template(states, key, &[]).await?;
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
            services
                .publisher
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
            states.set_agent_state(state.clone()).await?;
            let executed = execute_tools(
                states,
                tools,
                response.tool_calls.clone(),
                receipt,
                flags,
                cancellation,
            )
            .await;
            *state = states.get_agent_state().await?;
            if let Some(request) = executed? {
                context::compact_context(
                    states,
                    state,
                    services.provider,
                    definitions,
                    request,
                    cancellation,
                )
                .await?;
            }
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
                        start_event_id: opened.start_event_id().to_owned(),
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

impl TurnEngine {
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

    /// Prepares the turn and publishes its start event before runtime activation.
    /// The returned turn retains the published ID even if activation later fails.
    pub async fn open_turn<'a>(
        hooks: &'a dyn TurnHooks,
        publisher: &dyn EventPublisher,
        cancellation: CancellationToken<'_>,
    ) -> Result<OpenedTurn<'a>, Error> {
        let model = hooks.begin_conversation_turn().await?;
        let start_event_id = publisher.report_turn_started(&model, cancellation).await?;
        Ok(OpenedTurn {
            start_event_id,
            hooks,
        })
    }

    /// Runs one active turn using settings and tools captured at each step.
    /// Activates the published turn returned by `open_turn` before reading agent state.
    pub async fn run_turn(
        states: &mut dyn States,
        skill_provider: &dyn SkillProvider,
        tools: &dyn Tools,
        provider: &dyn Provider,
        publisher: &dyn EventPublisher,
        turn: &OpenedTurn<'_>,
        cancellation: CancellationToken<'_>,
    ) -> Result<Outcome, Error> {
        turn.hooks
            .activate_conversation_turn(turn.start_event_id())
            .await?;
        let mut state = states.get_agent_state().await?;
        let services = TurnServices {
            skills: skill_provider,
            provider,
            publisher,
        };
        let result = execute_turn(states, tools, &services, turn, cancellation, &mut state).await;
        states.set_agent_state(state).await?;
        result
    }

    /// Records an interrupted assistant turn through the same generic conversation operation.
    pub async fn interrupt_turn(
        states: &mut dyn States,
        start_event_id: &str,
    ) -> Result<Option<String>, Error> {
        let content =
            rendering::render_template(states, ConfigKey::InterruptedTemplate, &[]).await?;
        let usage = states.read_usage_totals().await?;
        let receipt = states
            .add_conversation_messages(ConversationAddition {
                messages: vec![Message::text(Role::Assistant, &content)],
                persistence: ConversationPersistence::CloseTurn {
                    start_event_id: start_event_id.to_owned(),
                    finish_reason: FinishReason::Interrupted,
                    usage,
                },
            })
            .await?;
        Ok(receipt.inserted.then_some(content))
    }

    /// Records a phase prompt only when it flips the agent state's final-phase marker:
    /// the final-provider-request prompt is recorded once, and the tool-availability
    /// restore is recorded only while the marker stands. A repeat records nothing.
    pub async fn add_phase_prompt(
        states: &mut dyn States,
        state: &mut AgentState,
        content: &str,
        final_phase: bool,
    ) -> Result<(), Error> {
        if state.final_phase == final_phase {
            return Ok(());
        }
        let (kind, notice) = if final_phase {
            (
                "final_provider_request_prompt",
                "Final provider request prompt injected",
            )
        } else {
            (
                "tool_availability_restored_prompt",
                "Tool availability restored prompt injected",
            )
        };
        states
            .add_conversation_messages(ConversationAddition {
                messages: vec![Message::text(Role::System, content)],
                persistence: ConversationPersistence::Append {
                    origin: ConversationOrigin::System,
                    event: ConversationEvent::Notice {
                        kind: kind.into(),
                        notice: notice.into(),
                    },
                },
            })
            .await?;
        state.final_phase = final_phase;
        Ok(())
    }

    pub async fn prepare_context_epoch(
        states: &mut dyn States,
        state: &mut AgentState,
        provider: &dyn Provider,
        definitions: &[ToolDefinition],
        cancellation: CancellationToken<'_>,
    ) -> Result<String, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let mut instructions = states.capture_system_prompt().await?;
        let mut messages = states.read_conversation_state().await?.assemble_messages();
        let mut configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
        let mut context = ContextSnapshot::estimate(estimate, &configuration);
        if context.requires_compaction(
            &state.last_request_statistic,
            &configuration.model.canonical_model,
        ) {
            let compacted = compact_context(
                states,
                state,
                provider,
                definitions,
                crate::CompactionRequest::default(),
                cancellation,
            )
            .await?;
            instructions = compacted.instructions;
            messages = states.read_conversation_state().await?.assemble_messages();
            configuration = states
                .get_config(ConfigKey::Context)
                .await?
                .into_context()?;
            let estimate = provider.estimate_input_tokens(&instructions, definitions, &messages)?;
            context = ContextSnapshot::estimate(estimate, &configuration);
            context.ensure_input_fits()?;
        }
        Ok(instructions)
    }
}
