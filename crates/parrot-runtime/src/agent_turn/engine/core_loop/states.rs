use super::conversation::{render_event, render_origin, render_record};
use super::*;
use parrot_core::{ConversationAddition, ConversationPersistence, ConversationReceipt};

pub(super) struct RuntimeStates<'a, S: AccessSnapshot + 'static> {
    pub(super) history: &'a AgentHistoryView,
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
}

impl<'a, S: AccessSnapshot + 'static> RuntimeStates<'a, S> {
    pub(super) fn bind(
        backend: &'a RuntimeBackend<S>,
        binding: &'a TurnBinding<S>,
        cancellation: &'a dyn Cancellation,
        work: &'a dyn DrainWork,
    ) -> Self {
        Self {
            history: &backend.history,
            activity: backend.activity.as_ref(),
            state: &backend.state,
            prompt: &backend.prompt,
            skills: backend.skills.as_ref(),
            status: backend.status.as_ref(),
            completion_sources: &backend.completion_sources,
            templates: &backend.templates,
            publisher: backend.publisher.as_ref(),
            usage: &backend.usage,
            compactor: &backend.compactor,
            event_id: &backend.event_id,
            events: ContextEmitter::new(
                &backend.history,
                backend.publisher.as_ref(),
                &backend.broker,
                backend.event_id.as_ref(),
            ),
            binding,
            cancellation,
            work,
        }
    }
}

impl<S> CoreStates for RuntimeStates<'_, S>
where
    S: AccessSnapshot + 'static,
{
    fn add_conversation_messages(
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
                | ConversationPersistence::StatusSteering { .. }) => {
                    return crate::compaction::add_conversation_messages(
                        self.history,
                        &mut state.conversation,
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
                persistence @ ConversationPersistence::ToolResult { .. } => {
                    return state
                        .tool_conversation
                        .add_conversation_messages(
                            &mut state.conversation,
                            ConversationAddition {
                                messages: addition.messages,
                                persistence,
                            },
                        )
                        .await;
                }
                ConversationPersistence::CompletionBlockerFeedback(feedback) => {
                    let mut candidate = Some(feedback.candidate);
                    for message in feedback.messages {
                        let event = render_event(
                            self.events.event(),
                            parrot_core::ConversationEvent::Notice {
                                kind: "completion_blocker".into(),
                                notice: "Completion blocker injected".into(),
                            },
                        );
                        if let Some(candidate) = candidate.take() {
                            self.history
                                .append_retained_reminder(
                                    event.clone(),
                                    render_record(
                                        &parrot_core::Message::from_response(&candidate),
                                        ConversationOrigin::Model,
                                    )?,
                                    message,
                                )
                                .await?;
                            self.publisher
                                .publish_committed(&Committed::after_commit(event), cancellation)
                                .await
                                .map_err(TurnError::Publication)?;
                        } else {
                            self.history.append_system_prompt(message).await?;
                            cancellation.check().map_err(|_| TurnError::Cancelled)?;
                            self.events.broker.publish_retained(&event);
                        }
                    }
                }
                ConversationPersistence::CloseTurn {
                    start_event_id,
                    finish_reason,
                    usage,
                } => {
                    self.work
                        .join_mutations()
                        .await
                        .map_err(|error| TurnError::Service(error.to_string()))?;
                    let key = TurnKey {
                        agent_session_id: self.history.agent_session_id().to_owned(),
                        start_event_id,
                    };
                    let mut event = self.events.event();
                    let completed: sessions::Completion = ProviderResponse {
                        finish_reason,
                        ..Default::default()
                    }
                    .into();
                    event.set_turn_ended(protocol::TurnEnded {
                        finish_reason: completed.finish_reason,
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        ..Default::default()
                    });
                    let record = render_record(&addition.messages[0], ConversationOrigin::Model)?;
                    let mut live = Vec::new();
                    for message in addition.messages {
                        live.push(crate::compaction::materialize_message(message));
                    }
                    let result = self
                        .history
                        .repository()
                        .events()
                        .close_turn_and_publish(
                            key,
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
                    .add(crate::compaction::materialize_message(message));
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
        adapt_operation(async move {
            let mut state = self.state.lock().await.agent_state.clone();
            state.final_phase = self
                .history
                .read_final_phase()
                .await
                .map_err(TurnError::from)?;
            state.standing_statuses = self
                .history
                .standing_statuses()
                .await
                .map_err(TurnError::from)?
                .unwrap_or_default();
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
            self.state.lock().await.agent_state = state;
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
                        reasoning: selection.model.canonical_model.reasoning(),
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

    fn collect_completion_blockers(&mut self) -> BoxFuture<'_, Result<Vec<String>, CoreError>> {
        adapt_operation(async move {
            let _guard = self.state.lock().await;
            self.work
                .join_mutations()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))?;
            let mut blockers = Vec::new();
            for source in self.completion_sources {
                if let Some(reminder) = source.get_pending_ask_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                }
            }
            for source in self.completion_sources {
                if let Some(reminder) = source.get_active_work_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                }
            }
            for source in self.completion_sources {
                if let Some(reminder) = source.get_exit_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                }
            }
            Ok(blockers)
        })
    }

    fn read_status_offering(
        &self,
        context: CoreContextSnapshot,
    ) -> BoxFuture<'_, Result<parrot_core::StatusOffering, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let status = self.status.bind(&selection);
            let slots = status
                .slots(&context, self.cancellation)
                .await
                .map_err(TurnError::Context)?;
            Ok(slots)
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

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            {
                let mut state = self.state.lock().await;
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

    fn read_tool_recovery(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ToolRecovery>, CoreError>> {
        adapt_operation(async move {
            self.state
                .lock()
                .await
                .tool_conversation
                .read_recovery()
                .await
        })
    }
    fn get_last_turn(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ConversationTurn>, CoreError>> {
        adapt_operation(async move { Ok(self.state.lock().await.read_last_turn()) })
    }
    fn promote_pending_inputs(
        &mut self,
        admission: parrot_core::InputAdmission,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::PromotedInput>, CoreError>> {
        adapt_operation(async move {
            let agent = self.history.agent_session_id().to_owned();
            let ids = self.event_id.clone();
            let compose = move |input: &parrot_storage::events::AdmittedInput| {
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
            };
            let mut promotions = self.history.promote_steers(compose.clone()).await?;
            if promotions.is_empty() && admission == parrot_core::InputAdmission::SteersOrNextQueued
            {
                promotions = self.history.promote_next_queue(compose).await?;
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
    fn read_usage_totals(&self) -> BoxFuture<'_, Result<parrot_core::UsageTotals, CoreError>> {
        adapt_operation(async move {
            let totals = self.usage.capture().own.totals;
            Ok(parrot_core::UsageTotals {
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
            })
        })
    }
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationState, CoreError>> {
        adapt_operation(async move {
            let state = self.state.lock().await;
            state
                .context
                .read_conversation_state(&state.conversation)
                .await
                .map_err(Into::into)
        })
    }
    fn finalize_compaction(
        &mut self,
        result: Result<Option<parrot_core::FittedCompaction>, CoreError>,
    ) -> BoxFuture<'_, Result<(), CoreError>> {
        Box::pin(async move {
            let compacted = result?;
            let mut guard = self.state.lock().await;
            let state = &mut *guard;
            state
                .context
                .record_epoch_compaction(&mut state.conversation, &self.events, Ok(compacted))
                .await
                .map_err(TurnError::from)
                .map_err(CoreError::from)
        })
    }
}
