use super::*;

impl<S, T> CoreStates for RuntimeStates<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn apply_conversation_addition(
        &mut self,
        addition: parrot_core::ConversationAddition,
    ) -> BoxFuture<'_, Result<parrot_core::ConversationReceipt, CoreError>> {
        adapt_operation(conversation::apply_conversation_addition(self, addition))
    }
    fn read_final_phase_marker(&self) -> BoxFuture<'_, Result<bool, CoreError>> {
        adapt_operation(async move {
            self.backend
                .history
                .read_final_phase()
                .await
                .map_err(Into::into)
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
                    let state = self.backend.state.lock().await;
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
                        self.backend
                            .compactor
                            .planner()
                            .capture_compaction_configuration(&selection.model, None),
                    ))
                }
                ConfigKey::SystemPromptBindings => self
                    .backend
                    .prompt
                    .read_core_prompt_bindings(self.cancellation)
                    .await
                    .map(ConfigValue::SystemPromptBindings)
                    .map_err(ContextError::from)
                    .map_err(Into::into),
                key => self
                    .backend
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
    fn read_turn_budget(&self) -> Result<parrot_core::TurnBudget, CoreError> {
        Ok(self.budget.clone())
    }
    fn write_turn_budget(&mut self, budget: parrot_core::TurnBudget) -> Result<(), CoreError> {
        *self.budget = budget;
        Ok(())
    }
    fn read_last_context_reminder_checkpoint(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::ContextReminderCheckpoint>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
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
            let _guard = self.backend.state.lock().await;
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
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_pending_ask_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::PendingAsks));
                }
            }
            for source in &self.backend.completion_sources {
                if let Some(reminder) = source.get_active_work_reminder(self.cancellation).await? {
                    blockers.push(reminder);
                    sources.push((source.clone(), CompletionFeedbackKind::ActiveWork));
                }
            }
            for source in &self.backend.completion_sources {
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
                .backend
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
    fn spill_group<'a, 'c>(
        &'a self,
        messages: &'a [parrot_core::Message],
        cancellation: parrot_core::CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::Message>, CoreError>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let mut state = self.backend.state.lock().await;
            let selection = self.binding.capture_selection().map_err(CoreError::from)?;
            crate::compaction::CompactionEstimates::new(&self.backend.compactor, &selection.model)
                .spill_group(state.context.borrow_blobs(), messages, cancellation)
                .map_err(Into::into)
        })
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
                &ContextEmitter {
                    backend: self.backend,
                },
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

    fn capture_system_prompt_sections(
        &mut self,
    ) -> BoxFuture<'_, Result<Vec<(String, String)>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection()?;
            {
                let mut state = backend.state.lock().await;
                state.request_status = Some(backend.status.bind(&selection));
                self.cancellation
                    .check()
                    .map_err(|_| ContextError::Cancelled)?;
                state
                    .context
                    .ensure_epoch(&backend.prompt, self.cancellation)
                    .await?;
            }
            backend
                .prompt
                .read_system_prompts(&selection.model, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_template_arguments<'a>(
        &'a self,
        source_key: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, serde_json::Value)>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_template_arguments(source_key, self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_agents_files(&self) -> BoxFuture<'_, Result<Vec<(String, String)>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_agents_files(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_agent_identity(
        &self,
    ) -> BoxFuture<'_, Result<Option<parrot_core::AgentIdentityDescription>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_agent_identity(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_standing_instruction(&self) -> BoxFuture<'_, Result<String, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_standing_instruction(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_skill_catalog(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::SkillCatalogEntry>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .prompt
                .read_skill_catalog(self.cancellation)
                .await
                .map_err(ContextError::from)
                .map_err(Into::into)
        })
    }
    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<parrot_core::ToolRecovery, CoreError>> {
        adapt_operation(async move {
            self.backend
                .state
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
            self.backend
                .state
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
            let state = self.backend.state.lock().await;
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
            let Some(input) = self.backend.history.peek_pending_input().await? else {
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
        adapt_operation(async move { self.backend.promote(admission).await })
    }
    fn resolve_skill_references<'a>(
        &'a self,
        references: &'a [parrot_core::SkillReference],
    ) -> BoxFuture<'a, Result<Vec<Option<parrot_core::Skill>>, CoreError>> {
        adapt_operation(async move {
            self.backend
                .skills
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
            self.backend
                .skills
                .read_skill_content(skill, self.cancellation)
                .await
                .map_err(TurnError::from)
        })
    }
    fn read_token_calibration(&self) -> BoxFuture<'_, Result<TokenCalibration, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state.context.budget().capture_state())
        })
    }
    fn read_provider_messages(
        &self,
    ) -> BoxFuture<'_, Result<Vec<parrot_core::Message>, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            Ok(state
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .map(Into::into)
                .collect())
        })
    }
    fn read_conversation_state(
        &self,
    ) -> BoxFuture<'_, Result<parrot_core::compaction::ConversationState, CoreError>> {
        adapt_operation(async move {
            let state = self.backend.state.lock().await;
            state
                .context
                .read_conversation_state(&state.conversation)
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
            let backend = self.backend;
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let events = ContextEmitter { backend };
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
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
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

