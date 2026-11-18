//! Turn engine: drains ready turns and drives one turn from input to settlement.

use crate::{
    AgentState, CancellationToken, CompactionRequest, CompletionBlockerFeedback, ConfigKey,
    ContextCompaction, ContextSnapshot, ConversationAddition, ConversationEvent,
    ConversationOrigin, ConversationPersistence, ConversationTurn, Error, ErrorKind,
    EventPublisher, FinishReason, InputAdmission, Message, Outcome, PromotedInput, Provider,
    ProviderRequest, Role, SkillProvider, States, StepOptions, ToolCallReceipt, ToolDefinition,
    Tools, TurnEngine, TurnHooks, TurnLifecycle, rendering, skills, stall, status::steer_status,
    tool_settlement::execute_tools,
};

/// Drives one opened turn with its runtime services, per-turn budget state and
/// skill preparation.
struct AgentTurn<'a> {
    states: &'a mut dyn States,
    tools: &'a dyn Tools,
    hooks: &'a dyn TurnHooks,
    skill_provider: &'a dyn SkillProvider,
    provider: &'a dyn Provider,
    event_publisher: &'a dyn EventPublisher,
    agent_state: AgentState,
    skill_preparation: skills::SkillPreparation,
}

/// The provider request assembled for one step, with its estimate facts.
struct PreparedRequest {
    request: ProviderRequest,
    estimated_input_tokens: i64,
    model_selector: String,
}

impl AgentTurn<'_> {
    /// Drives the turn from input admission through provider requests and tool
    /// settlement to completion, budget exhaustion or an error.
    async fn run(&mut self, cancellation: CancellationToken<'_>) -> Result<Outcome, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }

        self.agent_state.begin_turn();

        if self.agent_state.final_phase {
            let restored =
                rendering::render_template(self.states, ConfigKey::RestoredToolsTemplate, &[])
                    .await?;
            self.add_phase_prompt(&restored, false).await?;
        }

        let mut compacted_oversized = false;

        loop {
            if cancellation.is_cancelled() {
                return Err(Error::cancelled());
            }

            self.hooks.begin_step().await?;

            let turn_options = self.states.get_config(ConfigKey::Turn).await?.into_turn()?;
            let step_options = self.states.get_config(ConfigKey::Step).await?.into_step()?;
            let catalog = self.tools.capture_catalog().await?;
            let last_turn = self.states.get_last_turn().await?;
            let admission = if last_turn
                .as_ref()
                .is_some_and(ConversationTurn::needs_response)
            {
                InputAdmission::SteersOnly
            } else {
                InputAdmission::SteersOrNextQueued
            };
            let inputs = self.states.promote_pending_inputs(admission).await?;

            for input in &inputs {
                self.states
                    .add_conversation_messages(ConversationAddition {
                        messages: vec![input.message.clone()],
                        persistence: ConversationPersistence::Committed(input.publication.clone()),
                    })
                    .await?;
            }

            self.add_skill_prompt(&inputs, cancellation).await?;

            let Some(flags) = self.agent_state.admit(turn_options.maximum_requests) else {
                return self.exhaust_budget().await;
            };
            let definitions = if flags.final_phase {
                &catalog.final_phase
            } else {
                &catalog.normal
            };

            steer_status(self.states, &mut self.agent_state, cancellation).await?;

            if let Some(count) = stall::stall_index(&self.agent_state, turn_options.stall_steps) {
                let content = rendering::render_template(
                    self.states,
                    ConfigKey::StallTemplate,
                    &[("stall".into(), serde_json::Value::from(count.to_string()))],
                )
                .await?;
                self.states
                    .add_conversation_messages(ConversationAddition {
                        messages: vec![Message::text(Role::System, content)],
                        persistence: ConversationPersistence::Append {
                            origin: ConversationOrigin::System,
                            event: ConversationEvent::Notice {
                                kind: "stall".into(),
                                notice: format!("Stall prompt injected\nstall {count}"),
                            },
                        },
                    })
                    .await?;
            }

            if flags.final_phase {
                let content =
                    rendering::render_template(self.states, ConfigKey::FinalRequestTemplate, &[])
                        .await?;
                self.add_phase_prompt(&content, true).await?;
            }

            let prepared = self
                .prepare_request(definitions, &step_options, cancellation)
                .await?;

            let attempt = self.agent_state.count_request();
            self.states
                .set_agent_state(self.agent_state.clone())
                .await?;

            let invocation = self.provider.invoke(prepared.request, cancellation).await;

            let input_tokens = match &invocation {
                Ok(response) => Some(response.input_tokens),
                Err(error) => error.input_tokens,
            };
            if let Some(input_tokens) = input_tokens {
                self.agent_state.last_request_statistic.observe_usage(
                    &prepared.model_selector,
                    prepared.estimated_input_tokens,
                    input_tokens,
                );
            }

            let response = match invocation.map_err(|error| error.error) {
                Ok(response) => response,
                Err(error) if error.kind == ErrorKind::RequestTooLarge && !compacted_oversized => {
                    compacted_oversized = true;
                    if !self
                        .compact_context(
                            definitions,
                            CompactionRequest {
                                trigger: crate::CompactionTrigger::Automatic,
                                ..Default::default()
                            },
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

            let retry_reason = if response.finish_reason == FinishReason::Length
                && !response.tool_calls.is_empty()
            {
                Some(ConfigKey::TruncatedToolCallTemplate)
            } else if response.tool_calls.iter().any(|call| {
                serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err()
            }) {
                Some(ConfigKey::InvalidToolCallTemplate)
            } else {
                None
            };

            if let Some(key) = retry_reason {
                let content = rendering::render_template(self.states, key, &[]).await?;
                self.states
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
                let message_receipt = self
                    .states
                    .add_conversation_messages(ConversationAddition {
                        messages: vec![Message::from_response(&response)],
                        persistence: ConversationPersistence::Append {
                            origin: ConversationOrigin::Model,
                            event: ConversationEvent::Message,
                        },
                    })
                    .await?;

                self.event_publisher
                    .publish(
                        ConversationEvent::ToolRequestReceived {
                            count: response.tool_calls.len() as i32,
                        },
                        cancellation,
                    )
                    .await?;

                let receipt = ToolCallReceipt {
                    assistant_sequence: message_receipt.sequence,
                };
                self.states
                    .set_agent_state(self.agent_state.clone())
                    .await?;
                let settled = execute_tools(
                    self.states,
                    self.tools,
                    response.tool_calls.clone(),
                    receipt,
                    flags,
                    cancellation,
                )
                .await?;
                self.agent_state = self.states.get_agent_state().await?;
                stall::record_step(
                    &mut self.agent_state,
                    &response,
                    &settled.results,
                    !inputs.is_empty(),
                );
                self.states
                    .set_agent_state(self.agent_state.clone())
                    .await?;

                if let Some(request) = settled.compaction_request {
                    self.compact_context(definitions, request, cancellation)
                        .await?;
                }

                if flags.final_phase && !self.agent_state.try_extend_settlement() {
                    return self.exhaust_budget().await;
                }

                continue;
            }

            let messages = self.states.collect_completion_blockers().await?;

            if !messages.is_empty() {
                let mut additions = vec![Message::from_response(&response)];
                additions.extend(
                    messages
                        .iter()
                        .map(|message| Message::text(Role::System, message)),
                );

                self.states
                    .add_conversation_messages(ConversationAddition {
                        messages: additions,
                        persistence: ConversationPersistence::CompletionBlockerFeedback(
                            CompletionBlockerFeedback {
                                messages,
                                candidate: response.clone(),
                            },
                        ),
                    })
                    .await?;

                stall::record_step(&mut self.agent_state, &response, &[], !inputs.is_empty());
                self.agent_state.restart();

                continue;
            }

            let usage = self.states.read_usage_totals().await?;

            stall::record_step(&mut self.agent_state, &response, &[], !inputs.is_empty());

            self.states
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
    }

    /// Prepares the skill prompt for the promoted inputs and records it with its
    /// loaded-skill events when the prompt has content.
    async fn add_skill_prompt(
        &mut self,
        inputs: &[PromotedInput],
        cancellation: CancellationToken<'_>,
    ) -> Result<(), Error> {
        let skill_prompt = self
            .skill_preparation
            .prepare(self.states, self.skill_provider, inputs, cancellation)
            .await?;

        if skill_prompt.content.is_empty() {
            return Ok(());
        }

        self.states
            .add_conversation_messages(ConversationAddition {
                messages: vec![Message::text(Role::User, skill_prompt.content)],
                persistence: ConversationPersistence::Append {
                    origin: ConversationOrigin::System,
                    event: ConversationEvent::Notice {
                        kind: "skill_prompt".into(),
                        notice: String::new(),
                    },
                },
            })
            .await?;

        for path in skill_prompt.loaded_paths {
            self.event_publisher
                .publish(ConversationEvent::SkillLoaded { path }, cancellation)
                .await?;
        }

        Ok(())
    }

    /// Records a phase prompt only when it flips the agent state's final-phase marker:
    /// the final-provider-request prompt is recorded once, and the tool-availability
    /// restore is recorded only while the marker stands. A repeat records nothing.
    async fn add_phase_prompt(&mut self, content: &str, final_phase: bool) -> Result<(), Error> {
        if self.agent_state.final_phase == final_phase {
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

        self.states
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

        self.agent_state.final_phase = final_phase;

        Ok(())
    }

    /// Prepares the next provider request against the current conversation state,
    /// compacting first when the estimated input exceeds its trigger.
    async fn prepare_request(
        &mut self,
        definitions: &[ToolDefinition],
        options: &StepOptions,
        cancellation: CancellationToken<'_>,
    ) -> Result<PreparedRequest, Error> {
        let instructions = self
            .prepare_context_epoch(definitions, cancellation)
            .await?;

        let messages = self
            .states
            .read_conversation_state()
            .await?
            .assemble_messages();
        let context_configuration = self
            .states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate =
            self.provider
                .estimate_input_tokens(&instructions, definitions, &messages)?;
        let max_output_tokens = self
            .agent_state
            .last_request_statistic
            .calculate_maximum_output_tokens(&context_configuration, estimate)?;

        Ok(PreparedRequest {
            estimated_input_tokens: estimate,
            model_selector: context_configuration.canonical_model,
            request: ProviderRequest {
                model: options.model.clone(),
                instructions,
                messages,
                tools: definitions.to_vec(),
                max_output_tokens,
                reasoning: options.reasoning.clone(),
                provider_preferences: options.provider_preferences.clone(),
                include_router_metadata: options.include_router_metadata,
            },
        })
    }

    /// Estimates the current context, compacts once when it exceeds its trigger,
    /// and returns the instructions the request must use.
    async fn prepare_context_epoch(
        &mut self,
        definitions: &[ToolDefinition],
        cancellation: CancellationToken<'_>,
    ) -> Result<String, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }

        let mut instructions = self.states.capture_system_prompt().await?;
        let mut messages = self
            .states
            .read_conversation_state()
            .await?
            .assemble_messages();
        let mut context_configuration = self
            .states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let estimate =
            self.provider
                .estimate_input_tokens(&instructions, definitions, &messages)?;
        let mut snapshot = ContextSnapshot::estimate(estimate, &context_configuration);

        if snapshot.requires_compaction(
            &self.agent_state.last_request_statistic,
            &context_configuration.canonical_model,
        ) {
            let compacted = self
                .compact_context(
                    definitions,
                    CompactionRequest {
                        trigger: crate::CompactionTrigger::Automatic,
                        ..Default::default()
                    },
                    cancellation,
                )
                .await?;

            instructions = compacted.instructions;
            messages = self
                .states
                .read_conversation_state()
                .await?
                .assemble_messages();
            context_configuration = self
                .states
                .get_config(ConfigKey::Context)
                .await?
                .into_context()?;
            let estimate =
                self.provider
                    .estimate_input_tokens(&instructions, definitions, &messages)?;
            snapshot = ContextSnapshot::estimate(estimate, &context_configuration);
            snapshot.ensure_input_fits()?;
        }

        Ok(instructions)
    }

    /// Requests provider-owned compaction and reloads its state effects before
    /// returning.
    async fn compact_context(
        &mut self,
        definitions: &[ToolDefinition],
        mut request: CompactionRequest,
        cancellation: CancellationToken<'_>,
    ) -> Result<ContextCompaction, Error> {
        request.tools = definitions.to_vec();
        self.states
            .set_agent_state(self.agent_state.clone())
            .await?;
        let compacted = self.provider.compact(request, cancellation).await;
        self.agent_state = self.states.get_agent_state().await?;
        let reduced = compacted?.is_some();
        Ok(ContextCompaction {
            instructions: self.states.capture_system_prompt().await?,
            reduced,
        })
    }

    /// Renders the terminal notice core records when the invocation budget runs out.
    async fn exhaust_budget(&self) -> Result<Outcome, Error> {
        let notice =
            rendering::render_template(self.states, ConfigKey::RunawayTemplate, &[]).await?;
        Ok(Outcome::BudgetExhausted(notice))
    }

    /// Persists the agent state after execution, including failures.
    async fn record_state(self) -> Result<(), Error> {
        self.states.set_agent_state(self.agent_state).await
    }
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
            let mut turn = lifecycle.open_turn(token).await?;
            let result = async {
                let services = turn.borrow_services();
                services.hooks.activate_conversation_turn().await?;
                let agent_state = services.states.get_agent_state().await?;
                let mut agent_turn = AgentTurn {
                    states: services.states,
                    tools: services.tools,
                    hooks: services.hooks,
                    skill_provider: services.skills,
                    provider: services.provider,
                    event_publisher: services.event_publisher,
                    agent_state,
                    skill_preparation: skills::SkillPreparation::default(),
                };
                let outcome = agent_turn.run(token).await;
                agent_turn.record_state().await?;
                outcome
            }
            .await;
            turn.dispose(&result).await?;
            match result? {
                Outcome::Completed(completed) => answer = completed,
                exhausted @ Outcome::BudgetExhausted(_) => return Ok(exhausted),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AgentState, BoxFuture, CancellationToken, CompactionRequest, Content, ConversationAddition,
        ConversationPersistence, ConversationReceipt, Error, EventPublisher, Message, Provider,
        SkillProvider, States, ToolCatalog, ToolDefinition, ToolExecutionRequest, ToolResult,
        Tools, TurnHooks,
    };
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    fn finish<F: std::future::Future>(future: F) -> F::Output {
        let mut cx = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("phase effects resolve immediately"),
        }
    }

    /// Records every phase addition.
    #[derive(Default)]
    struct RecordingState {
        additions: Mutex<Vec<String>>,
    }

    impl States for RecordingState {
        fn get_config(
            &self,
            _: crate::ConfigKey,
        ) -> BoxFuture<'_, Result<crate::ConfigValue, Error>> {
            Box::pin(async { Err(Error::unsupported("configuration")) })
        }
        fn add_conversation_messages(
            &mut self,
            addition: ConversationAddition,
        ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
            Box::pin(async move {
                let ConversationPersistence::Append { .. } = addition.persistence else {
                    return Err(Error::unsupported("unexpected persistence"));
                };
                let Content::Text(text) = &addition.messages[0].contents[0] else {
                    return Err(Error::unsupported("unexpected content"));
                };
                self.additions.lock().unwrap().push(text.clone());
                Ok(ConversationReceipt::default())
            })
        }
    }

    #[test]
    fn a_phase_prompt_records_only_when_it_flips_the_durable_marker() {
        let mut states = RecordingState::default();
        let mut agent_turn = super::AgentTurn {
            states: &mut states,
            tools: &UnusedTools,
            hooks: &UnusedHooks,
            skill_provider: &UnusedSkills,
            provider: &UnusedProvider,
            event_publisher: &UnusedPublisher,
            agent_state: AgentState::default(),
            skill_preparation: crate::skills::SkillPreparation::default(),
        };
        finish(agent_turn.add_phase_prompt("final", true)).unwrap();
        finish(agent_turn.add_phase_prompt("duplicate", true)).unwrap();
        assert!(agent_turn.agent_state.final_phase);
        finish(agent_turn.add_phase_prompt("restored", false)).unwrap();
        finish(agent_turn.add_phase_prompt("duplicate restore", false)).unwrap();
        assert!(!agent_turn.agent_state.final_phase);
        drop(agent_turn);
        assert_eq!(
            *states.additions.lock().unwrap(),
            ["final".to_owned(), "restored".to_owned()]
        );
    }

    struct UnusedHooks;
    impl TurnHooks for UnusedHooks {
        fn begin_conversation_turn(&self) -> BoxFuture<'_, Result<crate::TurnModel, Error>> {
            Box::pin(async { Err(Error::unsupported("hooks")) })
        }
        fn activate_conversation_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async { Err(Error::unsupported("hooks")) })
        }
        fn begin_step(&self) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async { Err(Error::unsupported("hooks")) })
        }
    }

    struct UnusedTools;
    impl Tools for UnusedTools {
        fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
            Box::pin(async { Err(Error::unsupported("tools")) })
        }
        fn execute<'a>(
            &'a self,
            _: ToolExecutionRequest,
            _: CancellationToken<'a>,
        ) -> BoxFuture<'a, Result<Vec<ToolResult>, Error>> {
            Box::pin(async { Err(Error::unsupported("tools")) })
        }
    }

    struct UnusedProvider;
    impl Provider for UnusedProvider {}

    struct UnusedSkills;
    impl SkillProvider for UnusedSkills {}

    struct UnusedPublisher;
    impl EventPublisher for UnusedPublisher {}

    #[derive(Default)]
    struct CompactionCall {
        requests: Vec<CompactionRequest>,
        reply: Option<Result<Option<Message>, Error>>,
        effect: Option<AgentState>,
    }

    #[derive(Default)]
    struct Fixture {
        saved: AgentState,
        instructions: String,
        call: Arc<Mutex<CompactionCall>>,
    }

    impl States for Fixture {
        fn get_config(
            &self,
            _: crate::ConfigKey,
        ) -> BoxFuture<'_, Result<crate::ConfigValue, Error>> {
            Box::pin(async { Err(Error::unsupported("compaction fixture configuration")) })
        }

        fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
            Box::pin(async move {
                Ok(self
                    .call
                    .lock()
                    .unwrap()
                    .effect
                    .clone()
                    .unwrap_or_else(|| self.saved.clone()))
            })
        }

        fn set_agent_state(&mut self, state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
            Box::pin(async move {
                self.saved = state;
                Ok(())
            })
        }

        fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
            Box::pin(async move { Ok(self.instructions.clone()) })
        }
    }

    struct Compactor(Arc<Mutex<CompactionCall>>);

    impl Provider for Compactor {
        fn compact<'a, 'c>(
            &'a self,
            request: CompactionRequest,
            _cancellation: CancellationToken<'c>,
        ) -> BoxFuture<'a, Result<Option<Message>, Error>>
        where
            'c: 'a,
        {
            Box::pin(async move {
                let mut call = self.0.lock().unwrap();
                call.requests.push(request);
                call.reply.take().unwrap_or(Ok(None))
            })
        }
    }

    fn read_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            ..ToolDefinition::default()
        }
    }

    #[test]
    fn reloads_provider_state_on_success_and_post_commit_error() {
        for fail in [false, true] {
            let error = Error::failed("terminal publication failed after commit");
            let mut finalized = AgentState {
                final_phase: true,
                ..AgentState::default()
            };
            finalized
                .last_request_statistic
                .observe_usage("after/model", 30, 45);
            let mut fixture = Fixture {
                instructions: "fitted instructions".into(),
                ..Fixture::default()
            };
            {
                let mut call = fixture.call.lock().unwrap();
                call.effect = Some(finalized.clone());
                call.reply = Some(if fail {
                    Err(error.clone())
                } else {
                    Ok(Some(Message::default()))
                });
            }
            let mut agent_state = AgentState::default();
            agent_state
                .last_request_statistic
                .observe_usage("before/model", 10, 20);
            let compactor = Compactor(fixture.call.clone());
            let definitions = [read_tool()];
            let mut agent_turn = super::AgentTurn {
                states: &mut fixture,
                tools: &UnusedTools,
                hooks: &UnusedHooks,
                skill_provider: &UnusedSkills,
                provider: &compactor,
                event_publisher: &UnusedPublisher,
                agent_state,
                skill_preparation: crate::skills::SkillPreparation::default(),
            };
            let outcome = finish(agent_turn.compact_context(
                &definitions,
                CompactionRequest::default(),
                CancellationToken::uncancelled(),
            ));
            if fail {
                assert_eq!(outcome, Err(error));
            } else {
                assert!(outcome.unwrap().reduced);
            }
            assert_eq!(agent_turn.agent_state, finalized);
            drop(agent_turn);
            let call = fixture.call.lock().unwrap();
            assert_eq!(call.requests.len(), 1);
            assert_eq!(call.requests[0].tools, definitions);
        }
    }

    #[test]
    fn noop_keeps_calibration_and_returns_current_instructions() {
        let mut fixture = Fixture {
            instructions: "fresh instructions".into(),
            ..Fixture::default()
        };
        let mut agent_state = AgentState::default();
        agent_state
            .last_request_statistic
            .observe_usage("provider/model", 10, 40);
        let before = agent_state.clone();
        let compactor = Compactor(fixture.call.clone());
        let mut agent_turn = super::AgentTurn {
            states: &mut fixture,
            tools: &UnusedTools,
            hooks: &UnusedHooks,
            skill_provider: &UnusedSkills,
            provider: &compactor,
            event_publisher: &UnusedPublisher,
            agent_state,
            skill_preparation: crate::skills::SkillPreparation::default(),
        };
        let outcome = finish(agent_turn.compact_context(
            &[],
            CompactionRequest::default(),
            CancellationToken::uncancelled(),
        ))
        .unwrap();
        assert!(!outcome.reduced);
        assert_eq!(outcome.instructions, "fresh instructions");
        assert_eq!(agent_turn.agent_state, before);
    }
}
