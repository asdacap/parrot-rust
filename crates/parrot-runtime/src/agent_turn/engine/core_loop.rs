use super::*;
use parrot_core::{
    CompletionDecision as CoreCompletionDecision, ProviderError, Response, ResponseKind,
    RetryReason, State as CoreState, Tools as CoreTools,
};
use parrot_provider::routing::registry::ProviderModel;

struct RuntimeState<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    state: &'a mut State,
    cancellation: &'a dyn Cancellation,
    work: &'a dyn DrainWork,
}

struct RuntimeTools<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    cancellation: &'a dyn Cancellation,
}
struct RuntimeProvider<'a, S: AccessSnapshot, T: TurnTools<S>> {
    backend: &'a RuntimeBackend<S, T>,
    cancellation: &'a dyn Cancellation,
}

/// Grants tools access only to in-turn conversation and context mutations.
pub(super) trait ToolTurn: Send {
    /// Borrows mutations without exposing lifecycle, turn bookkeeping, or backend services.
    fn borrow_mutations(&mut self) -> ToolMutations<'_>;
}

pub(super) struct ToolMutations<'a> {
    pub(super) conversation: &'a mut AgentConversation,
    pub(super) context: &'a mut ContextController,
    request_status: Option<&'a Arc<dyn crate::compaction::ContextStatus>>,
}

impl ToolTurn for State {
    fn borrow_mutations(&mut self) -> ToolMutations<'_> {
        ToolMutations {
            conversation: &mut self.conversation,
            context: &mut self.context,
            request_status: self.request_status.as_ref(),
        }
    }
}

/// Grants a provider synchronous, per-completion token calibration only.
trait ProviderFeedback: Send + Sync {
    /// Calibrates input estimates before durable usage recording is awaited.
    fn observe_completion(
        &mut self,
        selector: &str,
        estimate: i64,
        completed: &sessions::Completion,
    );
}

impl ProviderFeedback for crate::compaction::ProviderTokenBudget {
    fn observe_completion(
        &mut self,
        selector: &str,
        estimate: i64,
        completed: &sessions::Completion,
    ) {
        self.observe_usage(selector, estimate, completed);
    }
}

struct UsageBridge<'a> {
    recorder: &'a SessionUsageRecorder,
    selected: &'a ProviderModel,
    feedback: &'a mut dyn ProviderFeedback,
    estimate: i64,
}
impl ProviderUsage for UsageBridge<'_> {
    fn observe_completion<'a>(
        &'a mut self,
        request: &'a sessions::Request,
        completed: &'a sessions::Completion,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            self.feedback
                .observe_completion(&self.selected.selector(), self.estimate, completed);
            self.recorder
                .record_request(self.selected, request.reasoning.as_ref(), completed)
                .await
        })
    }
}

struct PreparedRequest {
    request: sessions::Request,
    model: ProviderModel,
    estimate: i64,
}

pub(super) async fn run<S, T>(
    backend: &RuntimeBackend<S, T>,
    state: &mut State,
    cancellation: &dyn Cancellation,
    work: &dyn DrainWork,
) -> Result<Execution, TurnError>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    state.request_status = None;
    let mut state = RuntimeState {
        backend,
        state,
        cancellation,
        work,
    };
    let tools = RuntimeTools {
        backend,
        cancellation,
    };
    let provider = RuntimeProvider {
        backend,
        cancellation,
    };
    let outcome = parrot_core::run(&mut state, &tools, &provider).await;
    state.state.request_status = None;
    match outcome {
        Ok(parrot_core::Outcome::Completed(answer)) => Ok(Execution::succeeded(answer)),
        Ok(parrot_core::Outcome::Failed(message)) => Ok(Execution::failed(&message)),
        Err(parrot_core::Error::Adapter(error)) => Err(error),
        Err(parrot_core::Error::MissingTurn) => {
            Err(TurnError::Service("turn selection is unavailable".into()))
        }
    }
}

impl<S, T> CoreState for RuntimeState<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    type Selection = TurnSelection<S>;
    type Request = PreparedRequest;
    type Response = sessions::Completion;
    type ToolDefinition = sessions::Tool;
    type Error = TurnError;
    type ToolContext = dyn ToolTurn;
    type ProviderContext = dyn ProviderFeedback;

    fn borrow_tool_context(&mut self) -> &mut Self::ToolContext {
        self.state
    }
    fn borrow_provider_context(&mut self) -> &mut Self::ProviderContext {
        self.state.context.budget_mut()
    }
    fn disables_request_limit(&self) -> bool {
        self.backend.agent_limits.disable_max_turns
    }
    fn is_turn_open(&self) -> bool {
        self.state.turn.is_some()
    }
    fn has_scheduled_work(&self) -> bool {
        self.work.has_scheduled()
    }
    fn check_cancellation(&self) -> Result<(), TurnError> {
        self.cancellation.check().map_err(|_| TurnError::Cancelled)
    }
    fn has_work(&self) -> BoxFuture<'_, Result<bool, TurnError>> {
        Box::pin(async move {
            Ok(self.state.conversation.is_answerable()
                || self.backend.history.has_pending_inputs().await?)
        })
    }
    fn begin_turn(&mut self) -> BoxFuture<'_, Result<Self::Selection, TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let events = ContextEmitter { backend };
            backend.skills.end_turn().await;
            let mut selection = backend.selection.capture()?;
            backend.sessions.begin_turn().await?;
            let mut started = backend.event();
            let mut payload = protocol::TurnStarted {
                model: selection.model.canonical_model.selector(),
                ..Default::default()
            };
            if let Some(icon) = selection
                .model
                .alias
                .as_ref()
                .and_then(|alias| alias.icon.as_ref())
            {
                payload.model_alias_icon =
                    protobuf::MessageField::some(protocol::TurnModelAliasIcon {
                        glyph: icon.glyph.clone(),
                        color: protobuf::EnumOrUnknown::from_i32(icon.color as i32),
                        ..Default::default()
                    });
            }
            started.set_turn_started(payload);
            state.turn = Some(TurnKey {
                agent_session_id: backend.identity.session_id.clone(),
                start_event_id: started.id.clone(),
            });
            state.diagnostic_turn = Some(started.id.clone());
            backend.activity.turn_started(&started.id);
            backend.emit(started, cancellation).await?;
            state
                .context
                .ensure_epoch(&backend.prompt, cancellation)
                .await?;
            loop {
                let snapshot = backend.snapshot(&selection)?;
                let definitions = backend.tools.definitions(&snapshot).to_vec();
                let status = backend.status.bind(&selection);
                let outcome = state
                    .context
                    .inject_status(
                        EpochInput {
                            selection: &selection.model,
                            tools: &definitions,
                            conversation: &mut state.conversation,
                            prompt: &backend.prompt,
                            sessions: &backend.sessions,
                            status: status.as_ref(),
                            events: &events,
                        },
                        backend.identity.depth,
                        &selection.profile.id,
                        cancellation,
                    )
                    .await?;
                if matches!(outcome, StatusInjection::Complete) {
                    break;
                }
                selection = backend.selection.capture()?;
                tokio::task::yield_now().await;
            }
            backend.skills.begin_turn(cancellation).await?;
            Ok(selection)
        })
    }
    fn restore_tool_availability(&mut self) -> BoxFuture<'_, Result<(), TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let events = ContextEmitter { backend };
            self.state
                .context
                .restore_tool_availability(
                    &mut self.state.conversation,
                    &backend.notices.restored,
                    &events,
                    self.cancellation,
                )
                .await?;
            Ok(())
        })
    }
    fn promote_input(&mut self) -> BoxFuture<'_, Result<(), TurnError>> {
        Box::pin(self.backend.promote(self.state, self.cancellation))
    }
    fn maximum_requests(&self, selection: &Self::Selection) -> i32 {
        selection.profile.maximum_requests
    }
    fn take_interruption(&mut self) -> bool {
        self.backend.interruption.swap(false, Ordering::AcqRel)
    }
    fn prepare_request<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        definitions: &'a [Self::ToolDefinition],
        phase: parrot_core::budget::TurnRequestPhase,
    ) -> BoxFuture<'a, Result<Self::Request, TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let events = ContextEmitter { backend };
            if phase.enters_final_phase {
                let content = if phase.interrupted {
                    &backend.notices.interrupted_final
                } else {
                    &backend.notices.final_request
                };
                state
                    .context
                    .inject_final_provider_request_prompt(
                        &mut state.conversation,
                        content,
                        &events,
                        cancellation,
                    )
                    .await?;
            }
            let status = backend.status.bind(selection);
            state.request_status = Some(status.clone());
            let epoch = state
                .context
                .prepare_epoch(
                    EpochInput {
                        selection: &selection.model,
                        tools: definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    cancellation,
                )
                .await?;
            let messages: Vec<_> = state
                .conversation
                .messages()
                .iter()
                .map(provider_message)
                .collect();
            let estimate = crate::compaction::CompactionPlanner::estimate_input_tokens(
                &selection.model.canonical_model,
                &epoch.instructions,
                definitions,
                &messages,
            )
            .map_err(TurnError::Planning)?;
            let maximum = state
                .context
                .budget()
                .calculate_maximum_output_tokens(
                    &selection.model.canonical_model.selector(),
                    estimate,
                    &selection.model.canonical_model.model,
                )
                .map_err(TurnError::Planning)?;
            let request = sessions::Request {
                model: selection.model.canonical_model.model.id.clone(),
                instructions: epoch.instructions,
                messages,
                tools: definitions.to_vec(),
                max_output_tokens: maximum,
                reasoning: selection.model.canonical_model.reasoning(),
                ..Default::default()
            };
            Ok(PreparedRequest {
                request,
                model: selection.model.canonical_model.clone(),
                estimate,
            })
        })
    }
    fn compact_request<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        definitions: &'a [Self::ToolDefinition],
    ) -> BoxFuture<'a, Result<bool, TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let events = ContextEmitter { backend };
            let status = state
                .request_status
                .as_ref()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let compacted = state
                .context
                .compact_epoch(
                    EpochInput {
                        selection: &selection.model,
                        tools: definitions,
                        conversation: &mut state.conversation,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                    },
                    None,
                    cancellation,
                )
                .await?;
            Ok(compacted.reduced)
        })
    }
    fn record_retry(
        &mut self,
        reason: RetryReason,
        attempt: i32,
    ) -> BoxFuture<'_, Result<(), TurnError>> {
        let message = match reason {
            RetryReason::TruncatedToolCalls => &self.backend.notices.truncated,
            RetryReason::InvalidToolCalls => &self.backend.notices.invalid,
        };
        Box::pin(
            self.backend
                .retry_notice(self.state, message, attempt, self.cancellation),
        )
    }
    fn record_tool_calls<'a>(
        &'a mut self,
        completed: &'a Self::Response,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let published = backend.event();
            backend
                .history
                .append_conversation(published.clone(), conversation_message(completed))
                .await?;
            state.conversation.add(model_assistant(completed));
            backend.publish(published, cancellation).await?;
            backend
                .activity
                .assistant_message(&completed.assistant_text);
            let mut received = backend.event();
            received.set_tool_request_received(protocol::ToolRequestReceived {
                tool_call_count: completed.tool_calls.len() as i32,
                ..Default::default()
            });
            backend.emit(received, cancellation).await?;
            Ok(())
        })
    }
    fn evaluate_completion<'a>(
        &'a mut self,
        selection: &'a Self::Selection,
        completed: &'a Self::Response,
    ) -> BoxFuture<'a, Result<CoreCompletionDecision, TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let work = self.work;
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            let candidate = CompletionCandidate {
                session_id: backend.identity.session_id.clone(),
                message_id: parrot_domain::values::identifiers::Identifier::message_id()
                    .map_err(|error| TurnError::Service(error.to_string()))?,
                assistant: completed.clone().into_message(),
                selection: selection.clone(),
            };
            let mut retry = None;
            for callback in &backend.completions {
                match callback.complete(&candidate, cancellation).await? {
                    CompletionDecision::Complete => {}
                    decision @ CompletionDecision::Retry { .. } => {
                        retry = Some(decision);
                        break;
                    }
                }
            }
            if let Some(CompletionDecision::Retry {
                system_message,
                retain_candidate_assistant,
                completion_retry_pending,
            }) = retry
            {
                if retain_candidate_assistant {
                    state.conversation.add(model_assistant(completed));
                    backend
                        .activity
                        .assistant_message(&completed.assistant_text);
                }
                state
                    .conversation
                    .add(text_message(MessageRole::System, system_message));
                return Ok(CoreCompletionDecision::Retry {
                    pending: completion_retry_pending,
                });
            }
            Ok(CoreCompletionDecision::Complete)
        })
    }
    fn complete_turn<'a>(
        &'a mut self,
        completed: &'a Self::Response,
    ) -> BoxFuture<'a, Result<String, TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let state = &mut *self.state;
            let cancellation = self.cancellation;
            let work = self.work;
            work.join_mutations()
                .await
                .map_err(|e| TurnError::Service(e.to_string()))?;
            backend.close_turn(state, completed, cancellation).await?;
            if let Some(turn) = state.diagnostic_turn.take() {
                backend.activity.turn_finished(&turn, "completed", None);
            }
            backend
                .activity
                .assistant_message(&completed.assistant_text);
            Ok(completed.assistant_text.clone())
        })
    }
    fn fail_runaway(&mut self) -> BoxFuture<'_, Result<String, TurnError>> {
        Box::pin(async move {
            self.backend
                .fail(self.backend.notices.runaway.clone(), String::new())
                .await?;
            Ok(self.backend.notices.runaway.clone())
        })
    }
}

impl<S, T> CoreTools for RuntimeTools<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    type Context = dyn ToolTurn;
    type Selection = TurnSelection<S>;
    type Definition = sessions::Tool;
    type Snapshot = T::Snapshot;
    type Error = TurnError;

    fn reconcile<'a>(
        &'a self,
        context: &'a mut Self::Context,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            self.backend.reconcile(context, self.cancellation).await?;
            Ok(())
        })
    }
    fn snapshot(&self, selection: &TurnSelection<S>) -> Result<Self::Snapshot, TurnError> {
        self.backend.snapshot(selection)
    }
    fn definitions<'a>(&self, snapshot: &'a Self::Snapshot) -> &'a [sessions::Tool] {
        self.backend.tools.definitions(snapshot)
    }
    fn restrict_for_final_phase(&self, snapshot: &Self::Snapshot) -> Self::Snapshot {
        self.backend.tools.enabled_after_interruption(snapshot)
    }
    fn execute_pending<'a>(
        &'a self,
        context: &'a mut Self::Context,
        selection: &'a TurnSelection<S>,
        snapshot: &'a Self::Snapshot,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            let backend = self.backend;
            let mutations = context.borrow_mutations();
            let status = mutations
                .request_status
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let events = ContextEmitter { backend };
            backend
                .tools
                .reconcile(
                    TurnResources {
                        history: &backend.history,
                        conversation: mutations.conversation,
                        context: mutations.context,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                        usage: &backend.usage,
                    },
                    selection,
                    snapshot,
                    backend.selection.as_ref(),
                    self.cancellation,
                )
                .await
        })
    }
}

impl<S, T> parrot_core::Provider for RuntimeProvider<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    type Context = dyn ProviderFeedback;
    type Request = PreparedRequest;
    type Response = sessions::Completion;
    type Error = TurnError;

    fn invoke<'a>(
        &'a self,
        prepared: PreparedRequest,
        feedback: &'a mut Self::Context,
    ) -> BoxFuture<'a, Result<Response<sessions::Completion>, ProviderError<TurnError>>> {
        Box::pin(async move {
            let backend = self.backend;
            let session = backend
                .sessions
                .get(prepared.model.provider.clone())
                .await
                .map_err(|error| ProviderError::Failed(error.into()))?;
            let output = OutputBridge(backend.output.as_ref());
            let activity = ActivityBridge(backend.activity.as_ref());
            let mut usage = UsageBridge {
                recorder: &backend.usage,
                selected: &prepared.model,
                feedback,
                estimate: prepared.estimate,
            };
            let mut call = ProviderCall {
                history: &backend.history,
                publisher: backend.publisher.as_ref(),
                event_id: backend.event_id.as_ref(),
                output: &output,
                activity: &activity,
                usage: &mut usage,
            };
            let event = match call
                .stream(session.as_ref(), prepared.request, self.cancellation)
                .await
            {
                Ok(event) => event,
                Err(
                    error @ provider_call::ProviderCallError::Provider(
                        sessions::Error::Http(
                            parrot_provider::http_stream::HttpError::RequestTooLarge(_),
                        )
                        | sessions::Error::Socket(
                            parrot_provider::responses_websocket::WebSocketError::RequestTooLarge {
                                ..
                            },
                        ),
                    ),
                ) => return Err(ProviderError::RequestTooLarge(error.into())),
                Err(error) => return Err(ProviderError::Failed(error.into())),
            };
            let completed = match event.base_event() {
                sessions::Event::Completed(completed) => completed.clone(),
                _ => unreachable!("provider call returns completion"),
            };
            let kind = if completed.tool_calls.is_empty() {
                ResponseKind::Final
            } else if completed.finish_reason == "length" {
                ResponseKind::TruncatedToolCalls
            } else if completed.tool_calls.iter().any(|call| {
                serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_err()
            }) {
                ResponseKind::InvalidToolCalls
            } else {
                ResponseKind::ToolCalls
            };
            Ok(Response {
                value: completed,
                kind,
            })
        })
    }
}
