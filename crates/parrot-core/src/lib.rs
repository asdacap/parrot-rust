//! Turn orchestration with core-owned provider, tool, state, and event contracts.

pub mod budget;
pub mod compaction;
mod context;
mod conversation;
mod protocol;
pub mod rendering;
mod skills;
pub mod template;
pub use context::*;
pub use conversation::*;
pub use protocol::*;
pub use skills::{SkillMention, parse_skill_mentions};

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
            && states.get_pending_message().await?.is_none()
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
    let mut budget = states.read_turn_budget()?;
    let result = execute_turn(
        states,
        tools,
        provider,
        publisher,
        cancellation,
        &mut budget,
    )
    .await;
    states.write_turn_budget(budget)?;
    result
}

async fn execute_turn(
    states: &mut dyn States,
    tools: &dyn Tools,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    cancellation: CancellationToken<'_>,
    budget: &mut TurnBudget,
) -> Result<Outcome, Error> {
    if cancellation.is_cancelled() {
        return Err(Error::cancelled());
    }
    let options = states.get_config(ConfigKey::Turn).await?.into_turn()?;
    budget.begin_turn();
    let catalog = tools.capture_catalog().await?;
    if states.read_final_phase().await? {
        let restored =
            rendering::render_template(states, ConfigKey::RestoredToolsTemplate, &[]).await?;
        states
            .add_conversation(ConversationAddition {
                messages: vec![Message::text(Role::System, restored)],
                persistence: ConversationPersistence::Phase {
                    final_phase: false,
                    event: ConversationEvent::Notice {
                        kind: "tool_availability_restored_prompt".into(),
                        notice: "Tool availability restored prompt injected".into(),
                    },
                },
            })
            .await?;
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
        let inputs = states.promote_input(admission).await?;
        for input in &inputs {
            states
                .add_conversation(ConversationAddition {
                    messages: vec![input.message.clone()],
                    persistence: ConversationPersistence::Committed(input.publication.clone()),
                })
                .await?;
        }
        let context = skills.prepare(states, &inputs, cancellation).await?;
        if !context.content.is_empty() {
            states
                .add_conversation(ConversationAddition {
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
        let Some(flags) = budget.admit(options.maximum_requests) else {
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
        let attempt = budget.count_request();
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
                .add_conversation(ConversationAddition {
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
                .add_conversation(ConversationAddition {
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
                .add_conversation(ConversationAddition {
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
            let results = tools
                .execute(response.tool_calls.clone(), receipt, flags, cancellation)
                .await?;
            if results.is_empty() {
                return Err(Error::failed("tool execution returned no results"));
            }
            states.record_tool_results(&results).await?;
            if flags.final_phase && !budget.try_extend_settlement() {
                return Ok(Outcome::BudgetExhausted);
            }
            continue;
        }
        let messages = states.get_exit_blockers().await?;
        if messages.is_empty() {
            states
                .add_conversation(ConversationAddition {
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
            .add_conversation(ConversationAddition {
                messages: additions,
                persistence: ConversationPersistence::CompletionFeedback(CompletionFeedback {
                    messages,
                    candidate: response,
                }),
            })
            .await?;
        budget.restart();
    }
}

/// Records an interrupted assistant turn through the same generic conversation operation.
pub async fn interrupt_turn(states: &mut dyn States) -> Result<ConversationReceipt, Error> {
    let content = rendering::render_template(states, ConfigKey::InterruptedTemplate, &[]).await?;
    states
        .add_conversation(ConversationAddition {
            messages: vec![Message::text(Role::Assistant, content)],
            persistence: ConversationPersistence::CloseTurn {
                finish_reason: FinishReason::Interrupted,
            },
        })
        .await
}
