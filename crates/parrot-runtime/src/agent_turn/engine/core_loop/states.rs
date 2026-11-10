use super::conversation::{render_event, render_origin, render_record};
use super::*;
use parrot_core::{ConversationAddition, ConversationPersistence, ConversationReceipt};

pub(super) struct RuntimeStates<'a, S: AccessSnapshot> {
    pub(super) history: &'a AgentHistoryView,
    pub(super) sessions: &'a ProviderSessions,
    pub(super) activity: &'a dyn SessionActivity,
    pub(super) state: &'a Mutex<State>,
    pub(super) prompt: &'a SessionPrompt,
    pub(super) skills: &'a dyn TurnSkills,
    pub(super) status: &'a dyn TurnStatus<S>,
    pub(super) completion_sources: &'a [Arc<dyn CompletionStateSource>],
    pub(super) templates: &'a PromptTemplateCatalog,
    pub(super) publisher: &'a Publisher,
    pub(super) usage: &'a SessionUsageRecorder,
    pub(super) compactor: &'a crate::compaction::Compactor,
    pub(super) event_id: &'a Arc<dyn Fn() -> String + Send + Sync>,
    pub(super) events: ContextEmitter<'a>,
    pub(super) binding: &'a TurnBinding<S>,
    pub(super) cancellation: &'a dyn Cancellation,
    pub(super) work: &'a dyn DrainWork,
    pub(super) persistence_key: Option<TurnKey>,
    pub(super) agent_state: &'a mut parrot_core::AgentState,
}

impl<S> CoreStates for RuntimeStates<'_, S>
where
    S: AccessSnapshot + 'static,
{
    fn apply_conversation_addition(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, CoreError>> {
        adapt_operation(async move {
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            let mut sequence = 0;
            let mut publications = Vec::new();
            let cancellation = self.cancellation;
            match addition.persistence {
                ConversationPersistence::Append { origin, event } => {
                    for message in &addition.messages {
                        let event = render_event(self.events.event(), event.clone());
                        let item = self
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
                    return crate::compaction::apply_conversation_addition(
                        self.history,
                        &mut state.conversation,
                        &mut state.context.initial_status_pending,
                        ConversationAddition {
                            messages: addition.messages,
                            persistence,
                        },
                        &self.events,
                        cancellation,
                    )
                    .await
                    .map_err(Into::into);
                }
                persistence @ (ConversationPersistence::ToolResult { .. }
                | ConversationPersistence::ToolImages { .. }) => {
                    return state
                        .tool_conversation
                        .apply_conversation_addition(
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
                        &mut *self
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
                    self.work
                        .join_mutations()
                        .await
                        .map_err(|error| TurnError::Service(error.to_string()))?;
                    let key = self
                        .persistence_key
                        .as_ref()
                        .ok_or_else(|| TurnError::Service("turn start is unavailable".into()))?;
                    let totals = self.usage.capture().own.totals;
                    let mut event = self.events.event();
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
                        live.push(
                            crate::compaction::materialize_message(self.history, message).await?,
                        );
                    }
                    let result = self
                        .history
                        .repository()
                        .events()
                        .close_turn_and_publish(
                            key.clone(),
                            event,
                            Some(record),
                            &PublisherRef(self.publisher),
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
                        Err(CloseTurnError::Publication { committed, .. }) => {
                            Some(committed.as_ref())
                        }
                        _ => None,
                    };
                    if let Some(receipt) = committed {
                        self.history.refresh().await?;
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
                    self.activity.assistant_message(&text);
                }
                state
                    .conversation
                    .add(crate::compaction::materialize_message(self.history, message).await?);
            }
            for event in publications {
                self.publisher
                    .publish_committed(&Committed::after_commit(event), cancellation)
                    .await
                    .map_err(TurnError::Publication)?;
            }
            Ok(ConversationReceipt {
                inserted: true,
                sequence,
            })
        })
    }
    fn get_agent_state(&self) -> BoxFuture<'_, Result<parrot_core::AgentState, CoreError>> {
        let mut state = self.agent_state.clone();
        adapt_operation(async move {
            state.final_phase = self
                .history
                .read_final_phase()
                .await
                .map_err(TurnError::from)?;
            Ok(state)
        })
    }
    // The durable final-phase marker is only ever flipped by phase-prompt additions,
    // so saving the state never writes the marker durably.
    fn set_agent_state(
        &mut self,
        state: parrot_core::AgentState,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            *self.agent_state = state;
            Ok(())
        })
    }
    fn get_config(
        &self,
        key: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, CoreError>> {
        adapt_operation(async move {
            use parrot_core::{ConfigKey, ConfigValue};
            match key {
                ConfigKey::Turn => {
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Turn(TurnOptions {
                        maximum_requests: selection.profile.maximum_requests,
                        model: selection.model.canonical_model.model.id.clone(),
                        reasoning: selection.model.canonical_model.reasoning().map(Into::into),
                        provider_preferences: String::new(),
                        include_router_metadata: false,
                    }))
                }
                ConfigKey::Context => {
                    let state = self.state.lock().await;
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Context(
                        state
                            .context
                            .capture_context_configuration(&selection.model),
                    ))
                }
                ConfigKey::Compaction => {
                    let selection = self.binding.capture_selection()?;
                    Ok(ConfigValue::Compaction(
                        self.compactor
                            .planner()
                            .capture_compaction_configuration(&selection.model, None),
                    ))
                }
                key => self
                    .templates
                    .get_template(
                        crate::prompt_context::get_prompt_template_id(key).ok_or_else(|| {
                            TurnError::Service("unsupported template configuration key".into())
                        })?,
                    )
                    .map(ConfigValue::PromptTemplate)
                    .map_err(|error| {
                        TurnError::Prompt(crate::prompt_context::PromptError::Template(
                            error.to_string(),
                        ))
                    }),
            }
        })
    }
    fn read_last_context_reminder_checkpoint(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ContextReminderCheckpoint>, CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .get_last_context_reminder()
                .await
                .map_err(Into::into)
        })
    }

    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, CoreError>> {
        adapt_operation(async move {
            let _guard = self.state.lock().await;
            self.binding
                .exit_blocker_sources
                .lock()
                .expect("completion sources poisoned")
                .clear();
            let mut sources = Vec::new();
            self.work
                .join_mutations()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let mut blockers = Vec::new();
            for source in self.completion_sources {
                if let Some(reminder) = source.get_pending_ask_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::PendingAsks));
                }
            }
            for source in self.completion_sources {
                if let Some(reminder) = source.get_active_work_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::ActiveWork));
                }
            }
            for source in self.completion_sources {
                if let Some(reminder) = source.get_exit_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::ExitReminders));
                }
            }
            *self
                .binding
                .exit_blocker_sources
                .lock()
                .expect("completion sources poisoned") = sources;
            Ok(blockers)
        })
    }

    fn render_status<'a, 'c>(
        &'a self,
        context: CoreContextSnapshot,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, CoreError>>
    where
        'c: 'a,
    {
        adapt_operation(async move {
            let status = self
                .state
                .lock()
                .await
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            status
                .observe(
                    &crate::compaction::ContextSnapshot {
                        estimated_tokens: context.estimated_tokens,
                        context_limit: context.context_limit,
                        usage_percent: context.usage_percent,
                        trigger_percent: context.trigger_percent,
                        input_limit: context.input_limit,
                        trigger_tokens: context.trigger_tokens,
                        has_context_limit_override: context.has_context_limit_override,
                    },
                    &cancellation,
                )
                .await
                .map_err(Into::into)
        })
    }
    fn write_to_scratch<'a, 'c>(
        &'a self,
        contents: &'a [u8],
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.state
                .lock()
                .await
                .context
                .borrow_blobs()
                .write_to_scratch(contents, &cancellation)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(crate::compaction::CompactionError::from)
                .map_err(Into::into)
        })
    }
    fn read_file<'a, 'c>(
        &'a self,
        path: &'a str,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<u8>, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(
            async move { crate::compaction::read_file(path, &cancellation).map_err(Into::into) },
        )
    }

    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: parrot_core::compaction::CompactionProgress,
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            crate::compaction::CompactionObserver::observe_progress(
                &self.events,
                protocol::CompactionProgressEvent {
                    phase: protocol::CompactionPhase::COMPACTION_PHASE_SUMMARIZING.into(),
                    completed_groups: progress.completed_groups as u64,
                    total_groups: progress.total_groups as u64,
                    ..protocol::CompactionProgressEvent::new()
                },
                &cancellation,
            )
            .await
            .map_err(crate::compaction::CompactionError::from)
            .map_err(Into::into)
        })
    }

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            {
                let mut state = self.state.lock().await;
                state.request_status = Some(self.status.bind(&selection));
                self.cancellation
                    .check()
                    .map_err(|_| ContextError::Cancelled)?;
                state
                    .context
                    .ensure_epoch(self.prompt, self.cancellation)
                    .await?;
            }
            self.prompt
                .build(&selection.model, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }

    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<parrot_core::ToolRecovery, CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .tool_conversation
                .read_recovery()
                .await
        })
    }
    fn advance_tool_settlement_watermark(
        &mut self,
        through: i64,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .tool_conversation
                .advance_watermark(through);
            Ok(())
        })
    }
    fn get_last_turn(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationTurn>, CoreError>> {
        adapt_operation(async move {
            let state = self.state.lock().await;
            let Some(turn) = state.conversation.read_last_turn_messages() else {
                return Ok(None);
            };
            let messages: Vec<parrot_core::Message> =
                turn.iter().map(provider_message).map(Into::into).collect();
            let result = messages
                .iter()
                .rev()
                .find(|message| message.role != parrot_core::Role::System)
                .filter(|message| {
                    message.role == parrot_core::Role::Assistant && message.tool_calls.is_empty()
                })
                .cloned();
            Ok(Some(parrot_core::ConversationTurn { messages, result }))
        })
    }
    fn peek_pending_input_header(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::PendingMessage>, CoreError>> {
        adapt_operation(async move {
            let Some(input) = self.history.peek_pending_input().await? else {
                return Ok(None);
            };
            let delivery = match input.delivery {
                protocol::Delivery::DELIVERY_STEER => parrot_core::Delivery::Steer,
                protocol::Delivery::DELIVERY_QUEUE => parrot_core::Delivery::Queue,
                protocol::Delivery::DELIVERY_UNSPECIFIED => {
                    return Err(TurnError::Service(
                        "pending input delivery is unspecified".into(),
                    ));
                }
            };
            Ok(Some(parrot_core::PendingMessage {
                input_id: input.id,
                message_id: input.message_id,
                delivery,
            }))
        })
    }
    fn promote_pending_inputs(
        &mut self,
        admission: parrot_core::InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::PromotedInput>, CoreError>> {
        adapt_operation(async move {
            let agent = self.history.agent_session_id().to_owned();
            let ids = self.event_id.clone();
            let mut promotions = self
                .history
                .promote_steers(move |input| {
                    let mut event = Event {
                        id: ids(),
                        agent_session_id: agent.clone(),
                        ..Event::new()
                    };
                    event.set_input_promoted(protocol::InputPromoted {
                        input_id: input.id.clone(),
                        message_id: input.message_id.clone(),
                        ..Default::default()
                    });
                    Ok(event)
                })
                .await?;
            if promotions.is_empty() && admission == parrot_core::InputAdmission::SteersOrNextQueued
            {
                let agent = self.history.agent_session_id().to_owned();
                let ids = self.event_id.clone();
                promotions = self
                    .history
                    .promote_next_queue(move |input| {
                        let mut event = Event {
                            id: ids(),
                            agent_session_id: agent.clone(),
                            ..Event::new()
                        };
                        event.set_input_promoted(protocol::InputPromoted {
                            input_id: input.id.clone(),
                            message_id: input.message_id.clone(),
                            ..Default::default()
                        });
                        Ok(event)
                    })
                    .await?;
            }
            let mut inputs = Vec::with_capacity(promotions.len());
            for promotion in promotions {
                let contents = self.history.materialize(promotion.input.parts).await?;
                let message = ModelMessage {
                    role: MessageRole::User,
                    contents,
                    tool_calls: Vec::new(),
                    tool_call_id: String::new(),
                    provider_content: ProviderContent::default(),
                };
                inputs.push(parrot_core::PromotedInput {
                    input_id: promotion.input.id.clone(),
                    message: provider_message(&message).into(),
                    publication: parrot_core::ConversationPublication {
                        event_id: promotion.published.id,
                        agent_session_id: promotion.published.agent_session_id,
                        event: parrot_core::ConversationEvent::InputPromoted {
                            input_id: promotion.input.id,
                            message_id: promotion.input.message_id,
                        },
                    },
                });
            }
            Ok(inputs)
        })
    }
    fn resolve_skill_references<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<parrot_core::Skill>>, CoreError>> {
        adapt_operation(async move {
            self.skills
                .get_skills(references, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_skill_content<'a>(
        &'a self,
        skill: &'a parrot_core::Skill,
    ) -> BoxFuture<'a, Result<parrot_core::SkillContent, CoreError>> {
        adapt_operation(async move {
            self.skills
                .read_skill_content(skill, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, CoreError>> {
        adapt_operation(async move {
            let state = self.state.lock().await;
            Ok(state.context.budget().capture_state())
        })
    }
    fn read_provider_messages(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::Message>, CoreError>> {
        adapt_operation(async move {
            let state = self.state.lock().await;
            Ok(state
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .map(Into::into)
                .collect())
        })
    }
    fn read_conversation_summary(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationSummary>, CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .read_conversation_summary()
                .await
                .map_err(Into::into)
        })
    }
    fn read_conversation_tail(
        &self,
        after: i64,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::ConversationHistoryItem>, CoreError>> {
        adapt_operation(async move {
            let state = self.state.lock().await;
            state
                .context
                .read_conversation_tail(&state.conversation, after)
                .await
                .map_err(Into::into)
        })
    }
    fn read_checkpoint_sequences(&self) -> BoxFuture<'_, Result<Vec<i64>, CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .context
                .read_checkpoint_sequences()
                .await
                .map_err(Into::into)
        })
    }
    fn finalize_compaction<'a>(
        &'a mut self,
        instructions: &'a str,
        definitions: &'a [ToolDefinition],
        result: Result<Option<parrot_core::compaction::FittedCompaction>, CoreError>,
    ) -> BoxFuture<'a, Result<CoreContextCompaction, CoreError>> {
        Box::pin(async move {
            let definitions: Vec<sessions::Tool> =
                definitions.iter().cloned().map(Into::into).collect();

            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            let events = self.events;
            let compacted = match result {
                Ok(compacted) => compacted,
                Err(error) => {
                    state
                        .context
                        .fail_epoch_compaction(error.to_string(), &events)
                        .await
                        .map_err(TurnError::from)
                        .map_err(CoreError::from)?;
                    return Err(error);
                }
            };
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| CoreError::failed("request status is unavailable"))?;
            let result = state
                .context
                .record_epoch_compaction(
                    &mut EpochInput {
                        selection: &selection.model,
                        tools: &definitions,
                        conversation: &mut state.conversation,
                        prompt: self.prompt,
                        sessions: self.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    instructions.into(),
                    Ok(compacted),
                    self.cancellation,
                )
                .await
                .map_err(TurnError::from)
                .map_err(CoreError::from)?;
            Ok(CoreContextCompaction {
                instructions: result.instructions,
                reduced: result.reduced,
            })
        })
    }
}
