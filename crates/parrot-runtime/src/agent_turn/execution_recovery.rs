use super::*;
use parrot_domain::values::identifiers::Identifier;

enum RecoveryRequest {
    Resume,
    ResumeClaimed(Arc<dyn Cancellation>),
    Cancel(String),
}

impl<B: DrainBackend> AgentSession<B> {
    pub(super) async fn execution_checkpoint(
        &self,
    ) -> Result<Option<ExecutionCheckpoint>, LifecycleError> {
        match &self.inner.recovery {
            Some(store) => Ok(store
                .load()
                .await
                .map_err(storage_error)?
                .ok_or_else(|| {
                    LifecycleError::Failed("agent recovery snapshot is not initialized".into())
                })?
                .execution),
            None => Ok(None),
        }
    }

    pub(super) async fn prepare_execution(
        &self,
        parts: &[ConversationPart],
        message_id: &str,
    ) -> Result<ExecutionPreparation, LifecycleError> {
        let _gate = self.inner.checkpoint_gate.lock().await;
        if self.inner.recovery.is_some() {
            let checkpoint = self.execution_checkpoint().await?;
            if checkpoint
                .as_ref()
                .is_some_and(|saved| saved.result_claim.is_some())
            {
                return Err(LifecycleError::Failed(
                    "previous task result is still claimed".into(),
                ));
            }
            let existing = self
                .inner
                .backend
                .find_execution_input(message_id.to_owned())
                .await?;
            if let Some(input) = &existing {
                if input.parts != parts || input.delivery != Delivery::DELIVERY_STEER {
                    return Err(LifecycleError::Failed(format!(
                        "message {message_id} was already admitted with different content"
                    )));
                }
            }
            if let Some(saved) = &checkpoint {
                if saved.initial_message_id == message_id {
                    let original_parts: Vec<_> = saved
                        .initial_parts
                        .iter()
                        .map(|part| match part {
                            ExecutionInputPart::Text { text } => ConversationPart::text(text),
                            ExecutionInputPart::ImageArtifact {
                                artifact_id,
                                media_type,
                                display_name,
                            } => ConversationPart::ImageArtifact {
                                artifact_id: artifact_id.clone(),
                                media_type: media_type.clone(),
                                display_name: display_name.clone(),
                            },
                        })
                        .collect();
                    if original_parts != parts {
                        return Err(LifecycleError::Failed(format!(
                            "message {message_id} was already admitted with different content"
                        )));
                    }
                    if let Some(terminal) = &saved.terminal {
                        if let Some(input) = existing {
                            let outcome =
                                bounded(self.inner.backend.restore_execution(terminal).await?);
                            return Ok(ExecutionPreparation::Duplicate(
                                Box::new(Admission {
                                    input,
                                    published: None,
                                }),
                                Some(outcome),
                            ));
                        }
                    } else {
                        return Ok(ExecutionPreparation::Resume);
                    }
                }
                if saved.terminal.is_none() {
                    return Err(LifecycleError::Failed("previous execution is unfinished; recover it before admitting another execution".into()));
                }
            }
            if let Some(input) = existing {
                return Ok(ExecutionPreparation::Duplicate(
                    Box::new(Admission {
                        input,
                        published: None,
                    }),
                    None,
                ));
            }
        }
        self.begin_execution_locked(parts, message_id, ExecutionOwnership::Unclaimed)
            .await?;
        Ok(ExecutionPreparation::New)
    }

    pub(super) async fn begin_execution_locked(
        &self,
        parts: &[ConversationPart],
        message_id: &str,
        ownership: ExecutionOwnership,
    ) -> Result<(), LifecycleError> {
        let Some(store) = &self.inner.recovery else {
            return Ok(());
        };
        if let Some(mut previous) = self.execution_checkpoint().await? {
            if previous.result_claim.is_some() {
                return Err(LifecycleError::Failed(
                    "previous task result is still claimed".into(),
                ));
            }
            if previous.terminal.is_none() {
                return Err(LifecycleError::Failed("previous execution is unfinished; recover it before admitting another execution".into()));
            }
            if let Some(terminal) = &previous.terminal {
                if previous
                    .parent_notification
                    .as_ref()
                    .is_some_and(|notification| !notification.admitted)
                {
                    let outcome = bounded(self.inner.backend.restore_execution(terminal).await?);
                    self.inner
                        .backend
                        .execution_finished_correlated(&outcome, &previous.execution_id)
                        .await?;
                    if let Some(notification) = &mut previous.parent_notification {
                        notification.admitted = true;
                        notification.delivery_error = None;
                    }
                    store
                        .update(AgentStateUpdate::Execution(Some(previous)))
                        .await
                        .map_err(storage_error)?;
                }
            }
        }
        let execution_id =
            Identifier::event_id().map_err(|error| LifecycleError::Failed(error.to_string()))?;
        let checkpoint = ExecutionCheckpoint {
            result_claim: match ownership {
                ExecutionOwnership::Unclaimed => None,
                ExecutionOwnership::Claimed(claim) => Some(claim),
            },
            parent_notification: Some(NotificationCheckpoint {
                message_id: format!("{execution_id}.parent"),
                admitted: false,
                delivery_error: None,
            }),
            execution_id,
            initial_message_id: message_id.to_owned(),
            initial_parts: parts
                .iter()
                .map(|part| match part {
                    ConversationPart::Text(text) => ExecutionInputPart::Text { text: text.clone() },
                    ConversationPart::ImageArtifact {
                        artifact_id,
                        media_type,
                        display_name,
                    } => ExecutionInputPart::ImageArtifact {
                        artifact_id: artifact_id.clone(),
                        media_type: media_type.clone(),
                        display_name: display_name.clone(),
                    },
                })
                .collect(),
            history_watermark: self.inner.backend.execution_watermark().await?,
            phase: ExecutionPhase::AdmissionIntent,
            terminal: None,
        };
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint)))
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub(super) async fn activate_execution(
        &self,
        checkpoint: &ExecutionCheckpoint,
        owned: &Arc<OwnedReservation>,
        admission: &Admission,
    ) -> Result<bool, LifecycleError> {
        *owned.admission.lock().expect("reservation poisoned") = Some(admission.clone());
        let current = {
            let _gate = self.inner.checkpoint_gate.lock().await;
            let mut current = self.execution_checkpoint().await?.ok_or_else(|| {
                LifecycleError::Failed("execution disappeared during startup".into())
            })?;
            if current.execution_id != checkpoint.execution_id {
                return Err(LifecycleError::Failed(
                    "execution identity changed during startup".into(),
                ));
            }
            current.phase = if self.inner.interrupting.load(Ordering::Acquire)
                || current.phase == ExecutionPhase::Canceled
            {
                ExecutionPhase::Canceled
            } else if self.inner.suspending.load(Ordering::Acquire) {
                ExecutionPhase::Suspended
            } else {
                ExecutionPhase::Active
            };
            self.inner
                .recovery
                .as_ref()
                .expect("recovery store")
                .update(AgentStateUpdate::Execution(Some(current.clone())))
                .await
                .map_err(storage_error)?;
            current
        };
        if current.phase != ExecutionPhase::Active {
            return Ok(false);
        }
        self.inner
            .backend
            .execution_started_correlated(&current.execution_id)
            .await?;
        Ok(true)
    }

    pub(super) async fn persist_cancellation(&self) -> Result<(), LifecycleError> {
        let Some(store) = &self.inner.recovery else {
            return Ok(());
        };
        let _gate = self.inner.checkpoint_gate.lock().await;
        if let Some(mut checkpoint) = self.execution_checkpoint().await? {
            if checkpoint.terminal.is_none() {
                checkpoint.phase = ExecutionPhase::Canceled;
                store
                    .update(AgentStateUpdate::Execution(Some(checkpoint)))
                    .await
                    .map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(super) async fn persist_suspension(&self) -> Result<(), LifecycleError> {
        let Some(store) = &self.inner.recovery else {
            return Ok(());
        };
        let _gate = self.inner.checkpoint_gate.lock().await;
        if let Some(mut checkpoint) = self.execution_checkpoint().await? {
            if checkpoint.terminal.is_none() && checkpoint.phase != ExecutionPhase::Canceled {
                checkpoint.phase = ExecutionPhase::Suspended;
                store
                    .update(AgentStateUpdate::Execution(Some(checkpoint)))
                    .await
                    .map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(super) async fn finish_durable_execution(
        &self,
        execution: &mut Execution,
        reference: ExecutionResultReference,
    ) -> Result<bool, LifecycleError> {
        let Some(store) = &self.inner.recovery else {
            return Ok(false);
        };
        let _gate = self.inner.checkpoint_gate.lock().await;
        let Some(mut checkpoint) = self.execution_checkpoint().await? else {
            return Ok(false);
        };
        if self.inner.suspending.load(Ordering::Acquire)
            && execution.status == ExecutionStatus::Canceled
        {
            return Ok(true);
        }
        let terminal = self
            .inner
            .backend
            .capture_execution(&checkpoint, execution, reference)
            .await?;
        *execution = bounded(execution.clone());
        checkpoint.phase = if execution.status == ExecutionStatus::Canceled {
            ExecutionPhase::Canceled
        } else {
            ExecutionPhase::Completed
        };
        checkpoint.terminal = Some(terminal);
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint.clone())))
            .await
            .map_err(storage_error)?;
        // A failed parent admission leaves the actual outcome intact and the
        // notification pending. Resume retries with the same message identity.
        let delivery = self
            .inner
            .backend
            .execution_finished_correlated(execution, &checkpoint.execution_id)
            .await;
        if let Some(notification) = &mut checkpoint.parent_notification {
            match delivery {
                Ok(()) => {
                    notification.admitted = true;
                    notification.delivery_error = None;
                }
                Err(error) => {
                    let mut message = error.to_string();
                    bound(&mut message);
                    notification.delivery_error = Some(message);
                }
            }
        }
        store
            .update(AgentStateUpdate::Execution(Some(checkpoint)))
            .await
            .map_err(storage_error)?;
        Ok(true)
    }

    /// Reconstructs the retained public execution without submitting a new turn
    /// prompt. All scopes must be registered before composition activates it.
    pub fn recover_owned(&self) -> Result<ExecutionHandle, LifecycleError> {
        Ok(self.recover_owned_reserved()?.execution)
    }

    /// Reserves recovery and exposes readiness independently of provider completion.
    /// For saved terminals readiness follows exact result restoration; execution still
    /// joins parent delivery. Composition joins terminal/absent executions before activation.
    pub fn recover_owned_reserved(&self) -> Result<ClaimedExecutionHandle, LifecycleError> {
        self.start_owned_recovery(RecoveryRequest::Resume)
    }

    /// Cancels only the saved unfinished execution with this input identity.
    /// Startup is reserved before snapshot I/O; this agent's provider is never activated.
    /// Terminal results and parent completion delivery retain ordinary recovery behavior.
    pub fn cancel_recovered_execution(
        &self,
        expected_initial_message_id: &str,
    ) -> Result<ExecutionHandle, LifecycleError> {
        Ok(self
            .start_owned_recovery(RecoveryRequest::Cancel(
                expected_initial_message_id.to_owned(),
            ))?
            .execution)
    }

    fn start_owned_recovery(
        &self,
        request: RecoveryRequest,
    ) -> Result<ClaimedExecutionHandle, LifecycleError> {
        if self.inner.recovery.is_none() {
            return Err(LifecycleError::Failed(
                "owned recovery requires an agent state store".into(),
            ));
        }
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.start_owned_recovery_locked(&mut state, request)
    }

    pub(super) fn recover_claimed_locked(
        &self,
        state: &mut State<B>,
        caller: Arc<dyn Cancellation>,
    ) -> Result<ClaimedExecutionHandle, LifecycleError> {
        self.start_owned_recovery_locked(state, RecoveryRequest::ResumeClaimed(caller))
    }

    fn start_owned_recovery_locked(
        &self,
        state: &mut State<B>,
        request: RecoveryRequest,
    ) -> Result<ClaimedExecutionHandle, LifecycleError> {
        self.accepting(state)?;
        if matches!(&request, RecoveryRequest::Cancel(_))
            && ((state.started && !state.execution.is_finished()) || !state.tail.is_finished())
        {
            return Err(LifecycleError::Failed(
                "cannot cancel recovered execution while an execution is active".into(),
            ));
        }
        if state.started && !state.execution.is_finished() {
            return Ok(ClaimedExecutionHandle {
                execution: state.execution.clone(),
                reservation: state.execution_reservation.clone(),
            });
        }
        if state.drain.is_some() {
            return Err(LifecycleError::Failed(
                "cannot restore an execution while an unowned drain is active".into(),
            ));
        }
        state.started = true;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        state.execution = handle.clone();
        let reserved = Completion::new();
        let reservation = OperationHandle {
            completion: reserved.clone(),
        };
        state.execution_reservation = reservation.clone();
        let claim_ready = Completion::new();
        state.execution_claim = claim_ready.clone();
        let stop = Arc::new(Signal::new());
        state.execution_stop = Some(stop.clone());
        let cancellation: Arc<dyn Cancellation> = match &request {
            RecoveryRequest::ResumeClaimed(caller) => Arc::new(LinkedCancellation {
                lifetime: caller.clone(),
                stop,
                owner: None,
            }),
            _ => stop,
        };
        let owned = Arc::new(OwnedReservation {
            admission: Mutex::new(None),
            cancellation,
            startup: Completion::new(),
            result_reference: Mutex::new(ExecutionResultReference::Missing),
        });
        let selected = self.wake_locked(state, None, Some(owned.clone())).1;
        let session = self.clone();
        spawn_tracked(state, async move {
            let result = async {
                if let RecoveryRequest::Cancel(expected) = request {
                    session.cancel_checkpoint_input(&expected).await?;
                }
                session
                    .resume_checkpoint(owned.clone(), selected.clone(), reserved.clone())
                    .await
            }
            .await;
            if let Err(error) = &result {
                claim_ready.finish(Err(error.clone()));
                reserved.finish(Err(error.clone()));
            }
            owned.startup.finish(Ok(false));
            let _ = selected.join().await;
            completion.finish(result);
        });
        Ok(ClaimedExecutionHandle {
            execution: handle,
            reservation,
        })
    }

    async fn cancel_checkpoint_input(
        &self,
        expected_initial_message_id: &str,
    ) -> Result<(), LifecycleError> {
        let _gate = self.inner.checkpoint_gate.lock().await;
        let Some(mut checkpoint) = self.execution_checkpoint().await? else {
            return Ok(());
        };
        if checkpoint.terminal.is_some() {
            return Ok(());
        }
        if checkpoint.initial_message_id != expected_initial_message_id {
            return Err(LifecycleError::Failed(
                "saved unfinished execution does not match the input requested for cancellation"
                    .into(),
            ));
        }
        checkpoint.phase = ExecutionPhase::Canceled;
        self.inner
            .recovery
            .as_ref()
            .expect("recovery store")
            .update(AgentStateUpdate::Execution(Some(checkpoint)))
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub(super) async fn resume_checkpoint(
        &self,
        owned: Arc<OwnedReservation>,
        selected: ExecutionHandle,
        reserved: Arc<Completion<Result<(), LifecycleError>>>,
    ) -> Result<Execution, LifecycleError> {
        let saved = self.execution_checkpoint().await?;
        self.inner
            .state
            .lock()
            .expect("session poisoned")
            .execution_claim
            .finish(Ok(saved
                .as_ref()
                .and_then(|checkpoint| checkpoint.result_claim.clone())));
        let Some(checkpoint) = saved else {
            self.inner.state.lock().expect("session poisoned").started = false;
            reserved.finish(Ok(()));
            return Ok(Execution::succeeded(""));
        };
        if let Some(terminal) = &checkpoint.terminal {
            let execution = bounded(self.inner.backend.restore_execution(terminal).await?);
            reserved.finish(Ok(()));
            if checkpoint
                .parent_notification
                .as_ref()
                .is_some_and(|notification| !notification.admitted)
            {
                self.inner
                    .backend
                    .execution_finished_correlated(&execution, &checkpoint.execution_id)
                    .await?;
                let _gate = self.inner.checkpoint_gate.lock().await;
                let mut current = self.execution_checkpoint().await?.ok_or_else(|| {
                    LifecycleError::Failed("execution disappeared during parent delivery".into())
                })?;
                if current.execution_id != checkpoint.execution_id
                    || current.initial_message_id != checkpoint.initial_message_id
                    || current.terminal != checkpoint.terminal
                    || current
                        .parent_notification
                        .as_ref()
                        .map(|notification| &notification.message_id)
                        != checkpoint
                            .parent_notification
                            .as_ref()
                            .map(|notification| &notification.message_id)
                {
                    return Err(LifecycleError::Failed(
                        "execution identity changed during parent delivery".into(),
                    ));
                }
                if let Some(notification) = &mut current.parent_notification {
                    notification.admitted = true;
                    notification.delivery_error = None;
                }
                self.inner
                    .recovery
                    .as_ref()
                    .expect("recovery store")
                    .update(AgentStateUpdate::Execution(Some(current)))
                    .await
                    .map_err(storage_error)?;
            }
            return Ok(execution);
        }
        if checkpoint.phase == ExecutionPhase::Canceled {
            self.inner
                .backend
                .cancel_execution_input(&checkpoint.initial_message_id)
                .await?;
            owned.startup.finish(Ok(false));
            let _ = selected.join().await;
            let mut execution = Execution::canceled();
            self.finish_durable_execution(&mut execution, ExecutionResultReference::Missing)
                .await?;
            reserved.finish(Ok(()));
            return Ok(execution);
        }
        if self.inner.suspending.load(Ordering::Acquire) {
            reserved.finish(Err(shutting_down()));
            return Ok(Execution::canceled());
        }
        let parts = checkpoint
            .initial_parts
            .iter()
            .map(|part| match part {
                ExecutionInputPart::Text { text } => ConversationPart::text(text),
                ExecutionInputPart::ImageArtifact {
                    artifact_id,
                    media_type,
                    display_name,
                } => ConversationPart::ImageArtifact {
                    artifact_id: artifact_id.clone(),
                    media_type: media_type.clone(),
                    display_name: display_name.clone(),
                },
            })
            .collect();
        // SQLite admission is keyed by this exact message identity. The intent
        // can precede its commit, so reconcile it even for an active snapshot.
        let admission = self
            .inner
            .backend
            .admit(
                parts,
                checkpoint.initial_message_id.clone(),
                Delivery::DELIVERY_STEER,
                InputSource::INPUT_SOURCE_UNSPECIFIED,
            )
            .await?;
        let active = self
            .activate_execution(&checkpoint, &owned, &admission)
            .await?;
        self.inner
            .backend
            .publish_admission(&admission, &Signal::new())
            .await?;
        owned.startup.finish(Ok(active));
        reserved.finish(if self.inner.suspending.load(Ordering::Acquire) {
            Err(shutting_down())
        } else {
            Ok(())
        });
        let execution = self
            .finish_execution(selected.join().await, owned.capture_result_reference())
            .await;
        Ok(execution)
    }
}

pub(super) fn storage_error(
    error: parrot_domain::contracts::agent_state::AgentStateError,
) -> LifecycleError {
    LifecycleError::Failed(error.to_string())
}
