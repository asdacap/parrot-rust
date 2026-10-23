use super::*;
use parrot_domain::values::identifiers::Identifier;

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

    pub(super) async fn begin_execution(
        &self,
        parts: &[ConversationPart],
        message_id: &str,
    ) -> Result<(), LifecycleError> {
        let Some(store) = &self.inner.recovery else {
            return Ok(());
        };
        let _gate = self.inner.checkpoint_gate.lock().await;
        if let Some(mut previous) = self.execution_checkpoint().await? {
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
        if self.inner.recovery.is_none() {
            return Err(LifecycleError::Failed(
                "owned recovery requires an agent state store".into(),
            ));
        }
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        if state.started && !state.execution.is_finished() {
            return Ok(state.execution.clone());
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
        let owned = Arc::new(OwnedReservation {
            admission: Mutex::new(None),
            cancellation: Arc::new(Signal::new()),
            startup: Completion::new(),
            result_reference: Mutex::new(ExecutionResultReference::Missing),
        });
        let selected = self.wake_locked(&mut state, None, Some(owned.clone())).1;
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            let result = session
                .resume_checkpoint(owned.clone(), selected.clone())
                .await;
            owned.startup.finish(Ok(false));
            let _ = selected.join().await;
            completion.finish(result);
        });
        Ok(handle)
    }

    async fn resume_checkpoint(
        &self,
        owned: Arc<OwnedReservation>,
        selected: ExecutionHandle,
    ) -> Result<Execution, LifecycleError> {
        let Some(mut checkpoint) = self.execution_checkpoint().await? else {
            self.inner.state.lock().expect("session poisoned").started = false;
            return Ok(Execution::succeeded(""));
        };
        if let Some(terminal) = &checkpoint.terminal {
            let execution = bounded(self.inner.backend.restore_execution(terminal).await?);
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
                if let Some(notification) = &mut checkpoint.parent_notification {
                    notification.admitted = true;
                    notification.delivery_error = None;
                }
                self.inner
                    .recovery
                    .as_ref()
                    .expect("recovery store")
                    .update(AgentStateUpdate::Execution(Some(checkpoint)))
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
            return Ok(execution);
        }
        if self.inner.suspending.load(Ordering::Acquire) {
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
        let execution = self
            .finish_execution(selected.join().await, owned.capture_result_reference())
            .await;
        Ok(execution)
    }
}

fn storage_error(error: parrot_domain::contracts::agent_state::AgentStateError) -> LifecycleError {
    LifecycleError::Failed(error.to_string())
}
