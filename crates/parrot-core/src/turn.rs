//! Turn engine: drains ready turns and drives one turn from input to settlement.

use crate::{
    AgentState, CancellationToken, CompletionBlockerFeedback, ConfigKey, ConversationAddition,
    ConversationEvent, ConversationOrigin, ConversationPersistence, ConversationTurn, Error,
    ErrorKind, EventPublisher, FinishReason, InputAdmission, Message, Outcome, PromotedInput,
    Provider, Role, SkillProvider, States, ToolCallReceipt, Tools, TurnEngine, TurnHooks,
    TurnLifecycle, TurnServices, context, rendering, skills, status::steer_status,
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
    publisher: &'a dyn EventPublisher,
    state: AgentState,
    compacted_oversized: bool,
    skill_preparation: skills::SkillPreparation,
}

impl AgentTurn<'_> {
    /// Drives the turn from input admission through provider requests and tool
    /// settlement to completion, budget exhaustion or an error.
    async fn run(&mut self, cancellation: CancellationToken<'_>) -> Result<Outcome, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        self.state.begin_turn();
        if self.state.final_phase {
            let restored =
                rendering::render_template(self.states, ConfigKey::RestoredToolsTemplate, &[])
                    .await?;
            self.add_phase_prompt(&restored, false).await?;
        }
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::cancelled());
            }
            self.hooks.begin_step().await?;
            let options = self.states.get_config(ConfigKey::Turn).await?.into_turn()?;
            let catalog = self.tools.capture_catalog().await?;
            if cancellation.is_cancelled() {
                return Err(Error::cancelled());
            }
            let turn = self.states.get_last_turn().await?;
            let admission = if turn.as_ref().is_some_and(ConversationTurn::needs_response) {
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
            let Some(flags) = self.state.admit(options.maximum_requests) else {
                return self.exhaust_budget().await;
            };
            let definitions = if flags.final_phase {
                &catalog.final_phase
            } else {
                &catalog.normal
            };
            steer_status(
                self.states,
                &mut self.state,
                self.provider,
                definitions,
                cancellation,
            )
            .await?;
            if flags.final_phase {
                let content =
                    rendering::render_template(self.states, ConfigKey::FinalRequestTemplate, &[])
                        .await?;
                self.add_phase_prompt(&content, true).await?;
            }
            let prepared = context::prepare_request(
                self.states,
                &mut self.state,
                self.provider,
                definitions,
                &options,
                cancellation,
            )
            .await?;
            let attempt = self.state.count_request();
            self.states.set_agent_state(self.state.clone()).await?;
            let invoked = self.provider.invoke(prepared.request, cancellation).await;
            let input_tokens = match &invoked {
                Ok(response) => Some(response.input_tokens),
                Err(error) => error.input_tokens,
            };
            if let Some(input_tokens) = input_tokens {
                self.state.last_request_statistic.observe_usage(
                    &prepared.model_selector,
                    prepared.estimated_input_tokens,
                    input_tokens,
                );
            }
            let response = match invoked.map_err(|error| error.error) {
                Ok(response) => response,
                Err(error)
                    if error.kind == ErrorKind::RequestTooLarge && !self.compacted_oversized =>
                {
                    self.compacted_oversized = true;
                    if !context::compact_context(
                        self.states,
                        &mut self.state,
                        self.provider,
                        definitions,
                        crate::CompactionRequest {
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
            self.compacted_oversized = false;
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
                let recorded = self
                    .states
                    .add_conversation_messages(ConversationAddition {
                        messages: vec![Message::from_response(&response)],
                        persistence: ConversationPersistence::Append {
                            origin: ConversationOrigin::Model,
                            event: ConversationEvent::Message,
                        },
                    })
                    .await?;
                self.publisher
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
                self.states.set_agent_state(self.state.clone()).await?;
                let compaction_request = execute_tools(
                    self.states,
                    self.tools,
                    response.tool_calls.clone(),
                    receipt,
                    flags,
                    cancellation,
                )
                .await?;
                self.state = self.states.get_agent_state().await?;
                if let Some(request) = compaction_request {
                    context::compact_context(
                        self.states,
                        &mut self.state,
                        self.provider,
                        definitions,
                        request,
                        cancellation,
                    )
                    .await?;
                }
                if flags.final_phase && !self.state.try_extend_settlement() {
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
                                candidate: response,
                            },
                        ),
                    })
                    .await?;
                self.state.restart();
                continue;
            }
            let usage = self.states.read_usage_totals().await?;
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
            self.publisher
                .publish(ConversationEvent::SkillLoaded { path }, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Records a phase prompt only when it flips the agent state's final-phase marker:
    /// the final-provider-request prompt is recorded once, and the tool-availability
    /// restore is recorded only while the marker stands. A repeat records nothing.
    async fn add_phase_prompt(&mut self, content: &str, final_phase: bool) -> Result<(), Error> {
        if self.state.final_phase == final_phase {
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
        self.state.final_phase = final_phase;
        Ok(())
    }

    /// Renders the terminal notice core records when the invocation budget runs out.
    async fn exhaust_budget(&self) -> Result<Outcome, Error> {
        let notice =
            rendering::render_template(self.states, ConfigKey::RunawayTemplate, &[]).await?;
        Ok(Outcome::BudgetExhausted(notice))
    }

    /// Persists the agent state after execution, including failures.
    async fn record_state(self) -> Result<(), Error> {
        self.states.set_agent_state(self.state).await
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
            let result = run_opened_turn(turn.borrow_services(), token).await;
            turn.dispose(&result).await?;
            match result? {
                Outcome::Completed(completed) => answer = completed,
                exhausted @ Outcome::BudgetExhausted(_) => return Ok(exhausted),
            }
        }
    }
}

async fn run_opened_turn(
    turn: TurnServices<'_>,
    cancellation: CancellationToken<'_>,
) -> Result<Outcome, Error> {
    turn.hooks.activate_conversation_turn().await?;
    let state = turn.states.get_agent_state().await?;
    let mut agent = AgentTurn {
        states: turn.states,
        tools: turn.tools,
        hooks: turn.hooks,
        skill_provider: turn.skills,
        provider: turn.provider,
        publisher: turn.publisher,
        state,
        compacted_oversized: false,
        skill_preparation: skills::SkillPreparation::default(),
    };
    let result = agent.run(cancellation).await;
    agent.record_state().await?;
    result
}

#[cfg(test)]
mod tests {
    use crate::{
        AgentState, BoxFuture, CancellationToken, Content, ConversationAddition,
        ConversationPersistence, ConversationReceipt, Error, EventPublisher, Provider,
        SkillProvider, States, ToolCatalog, ToolExecutionRequest, ToolResult, Tools, TurnHooks,
    };
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    fn finish<F: std::future::Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
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
        let mut agent = super::AgentTurn {
            states: &mut states,
            tools: &UnusedTools,
            hooks: &UnusedHooks,
            skill_provider: &UnusedSkills,
            provider: &UnusedProvider,
            publisher: &UnusedPublisher,
            state: AgentState::default(),
            compacted_oversized: false,
            skill_preparation: crate::skills::SkillPreparation::default(),
        };
        finish(agent.add_phase_prompt("final", true)).unwrap();
        finish(agent.add_phase_prompt("duplicate", true)).unwrap();
        assert!(agent.state.final_phase);
        finish(agent.add_phase_prompt("restored", false)).unwrap();
        finish(agent.add_phase_prompt("duplicate restore", false)).unwrap();
        assert!(!agent.state.final_phase);
        drop(agent);
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
}
