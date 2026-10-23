use super::*;

impl<B: DrainBackend> AgentSession<B> {
    async fn check_unclaimed_admission(&self) -> Result<(), LifecycleError> {
        if self
            .execution_checkpoint()
            .await?
            .is_some_and(|checkpoint| checkpoint.result_claim.is_some())
        {
            return Err(LifecycleError::Failed(
                "previous task result is still claimed".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn wake_unclaimed(
        &self,
        activity: Option<IncomingActivity>,
    ) -> (bool, ExecutionHandle) {
        let mut state = self.inner.state.lock().expect("session poisoned");
        if self.inner.recovery.is_none() || state.drain.is_some() {
            return self.wake_locked(&mut state, activity, None);
        }
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            let prepared = async {
                let gate = session.inner.checkpoint_gate.lock().await;
                session.check_unclaimed_admission().await?;
                let selected = {
                    let mut state = session.inner.state.lock().expect("session poisoned");
                    session.accepting(&state)?;
                    session.wake_locked(&mut state, activity, None).1
                };
                drop(gate);
                Ok(selected)
            };
            let selected = tokio::select! {
                result = prepared => result,
                _ = session.inner.closing.cancelled() => { completion.finish(Ok(Execution::canceled())); return; },
                _ = session.inner.lifetime.cancelled() => { completion.finish(Ok(Execution::canceled())); return; },
            };
            completion.finish(match selected {
                Ok(selected) => selected.join().await,
                Err(error) => Err(error),
            });
        });
        (true, handle)
    }

    /// Admits a task prompt with its result claim in the same durable intent.
    /// A foreign claim fails before reserving a drain or successor FIFO position.
    pub fn enqueue_claimed_execution(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        claim: TaskResultClaim,
        caller: Arc<dyn Cancellation>,
    ) -> Result<ClaimedExecutionHandle, LifecycleError> {
        check_message(&parts)?;
        if self.inner.recovery.is_none() {
            return Err(LifecycleError::Failed(
                "claimed execution requires an agent state store".into(),
            ));
        }
        if claim.claim_id != message_id {
            return Err(LifecycleError::Failed(
                "task claim must match its prompt identity".into(),
            ));
        }
        let mut state = self.inner.state.lock().expect("session poisoned");
        self.accepting(&state)?;
        let completion = Completion::new();
        let handle = OperationHandle {
            completion: completion.clone(),
        };
        let reserved = Completion::new();
        let reservation = OperationHandle {
            completion: reserved.clone(),
        };
        let session = self.clone();
        spawn_tracked(&mut state, async move {
            let outcome = session
                .execute_claimed(
                    parts,
                    message_id,
                    claim,
                    caller,
                    completion.clone(),
                    reserved.clone(),
                )
                .await;
            if let Err(error) = &outcome {
                reserved.finish(Err(error.clone()));
            }
            completion.finish(outcome);
        });
        Ok(ClaimedExecutionHandle {
            execution: handle,
            reservation,
        })
    }

    async fn execute_claimed(
        &self,
        parts: Vec<ConversationPart>,
        message_id: String,
        claim: TaskResultClaim,
        caller: Arc<dyn Cancellation>,
        completion: Arc<Completion<Result<Execution, LifecycleError>>>,
        reserved: Arc<Completion<Result<(), LifecycleError>>>,
    ) -> Result<Execution, LifecycleError> {
        loop {
            let gate = self.inner.checkpoint_gate.lock().await;
            if let Some(checkpoint) = self.execution_checkpoint().await? {
                if checkpoint.initial_message_id == message_id
                    && checkpoint.result_claim.as_ref() == Some(&claim)
                {
                    let (active, restored) = {
                        let mut state = self.inner.state.lock().expect("session poisoned");
                        if state.started && !state.execution.is_finished() {
                            (
                                Some((state.execution.clone(), state.execution_claim.clone())),
                                None,
                            )
                        } else {
                            (None, Some(self.recover_owned_locked(&mut state)?))
                        }
                    };
                    drop(gate);
                    if let Some(restored) = restored {
                        reserved.finish(Ok(()));
                        return restored.join().await;
                    }
                    if let Some((active, ready)) = active {
                        if ready.wait().await?.as_ref() == Some(&claim) {
                            reserved.finish(Ok(()));
                            return active.join().await;
                        }
                        let _ = active.join().await;
                        continue;
                    }
                    unreachable!("matching recovery selects an active or newly reserved execution");
                }
                if checkpoint.result_claim.is_some() {
                    return Err(LifecycleError::Failed(
                        "previous task result is still claimed".into(),
                    ));
                }
                if checkpoint.terminal.is_none() {
                    return Err(LifecycleError::Failed("previous execution is unfinished; recover it before admitting another execution".into()));
                }
            }
            if caller.is_cancelled() {
                return Err(canceled());
            }
            let (owned, selected) = {
                let mut state = self.inner.state.lock().expect("session poisoned");
                self.accepting(&state)?;
                if state.drain.is_some()
                    || !state.tail.is_finished()
                    || (state.started && !state.execution.is_finished())
                {
                    return Err(LifecycleError::Failed("agent execution is busy".into()));
                }
                self.inner.interrupting.store(false, Ordering::Release);
                state.started = true;
                state.execution = OperationHandle {
                    completion: completion.clone(),
                };
                state.execution_claim = Completion::ready(Ok(Some(claim.clone())));
                let owned = Arc::new(OwnedReservation {
                    admission: Mutex::new(None),
                    cancellation: caller,
                    startup: Completion::new(),
                    result_reference: Mutex::new(ExecutionResultReference::Missing),
                });
                let selected = self.wake_locked(&mut state, None, Some(owned.clone())).1;
                (owned, selected)
            };
            let prepared = self
                .begin_execution_locked(&parts, &message_id, ExecutionOwnership::Claimed(claim))
                .await;
            drop(gate);
            if let Err(error) = prepared {
                owned.startup.finish(Ok(false));
                let _ = selected.join().await;
                return Err(error);
            }
            reserved.finish(if self.inner.suspending.load(Ordering::Acquire) {
                Err(shutting_down())
            } else {
                Ok(())
            });
            return self
                .execute_prepared_owned(parts, message_id, None, owned, selected)
                .await;
        }
    }

    /// Captures the exact terminal still protected by this task's claim.
    pub fn capture_claimed_result<'a>(
        &'a self,
        expected_prompt_id: &'a str,
        claim: &'a TaskResultClaim,
    ) -> BoxFuture<'a, Result<TaskResponseReference, LifecycleError>> {
        Box::pin(async move {
            let _gate = self.inner.checkpoint_gate.lock().await;
            let snapshot = self
                .inner
                .recovery
                .as_ref()
                .ok_or_else(|| {
                    LifecycleError::Failed("claimed execution requires an agent state store".into())
                })?
                .load()
                .await
                .map_err(recovery::storage_error)?
                .ok_or_else(|| {
                    LifecycleError::Failed("agent recovery snapshot is not initialized".into())
                })?;
            let checkpoint = snapshot
                .execution
                .ok_or_else(|| LifecycleError::Failed("claimed execution is missing".into()))?;
            if checkpoint.initial_message_id != expected_prompt_id
                || checkpoint.result_claim.as_ref() != Some(claim)
            {
                return Err(LifecycleError::Failed(
                    "execution does not match the task result claim".into(),
                ));
            }
            let terminal = checkpoint.terminal.ok_or_else(|| {
                LifecycleError::Failed("claimed execution has not finished".into())
            })?;
            Ok(TaskResponseReference {
                worker_session_id: snapshot.owner_session_id,
                initial_message_id: checkpoint.initial_message_id,
                execution_id: checkpoint.execution_id,
                terminal,
            })
        })
    }

    /// Releases only the exact result already saved by the owning task graph.
    pub fn acknowledge_claimed_result<'a>(
        &'a self,
        reference: &'a TaskResponseReference,
        claim: &'a TaskResultClaim,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        Box::pin(async move {
            let _gate = self.inner.checkpoint_gate.lock().await;
            let store = self.inner.recovery.as_ref().ok_or_else(|| {
                LifecycleError::Failed("claimed execution requires an agent state store".into())
            })?;
            let snapshot = store
                .load()
                .await
                .map_err(recovery::storage_error)?
                .ok_or_else(|| {
                    LifecycleError::Failed("agent recovery snapshot is not initialized".into())
                })?;
            if snapshot.owner_session_id != reference.worker_session_id
                || reference.initial_message_id != claim.claim_id
            {
                return Err(LifecycleError::Failed(
                    "task response reference belongs to another worker or claim".into(),
                ));
            }
            let Some(mut current) = snapshot.execution else {
                return Ok(());
            };
            if current.execution_id != reference.execution_id {
                return Ok(());
            }
            if current.initial_message_id != reference.initial_message_id
                || current.terminal.as_ref() != Some(&reference.terminal)
                || current
                    .result_claim
                    .as_ref()
                    .is_some_and(|current| current != claim)
            {
                return Err(LifecycleError::Failed(
                    "task response acknowledgment conflicts with the saved execution".into(),
                ));
            }
            if current.result_claim.is_some() {
                current.result_claim = None;
                store
                    .update(AgentStateUpdate::Execution(Some(current)))
                    .await
                    .map_err(recovery::storage_error)?;
            }
            Ok(())
        })
    }

    /// Restores a previously saved exact response even after a newer execution.
    pub fn restore_saved_response<'a>(
        &'a self,
        reference: &'a TaskResponseReference,
    ) -> BoxFuture<'a, Result<Execution, LifecycleError>> {
        Box::pin(async move {
            let store = self.inner.recovery.as_ref().ok_or_else(|| {
                LifecycleError::Failed("saved response requires an agent state store".into())
            })?;
            let snapshot = store
                .load()
                .await
                .map_err(recovery::storage_error)?
                .ok_or_else(|| {
                    LifecycleError::Failed("agent recovery snapshot is not initialized".into())
                })?;
            if snapshot.owner_session_id != reference.worker_session_id
                || reference.terminal.event_id != format!("{}.finished", reference.execution_id)
            {
                return Err(LifecycleError::Failed(
                    "task response reference belongs to another worker or execution".into(),
                ));
            }
            Ok(bounded(
                self.inner
                    .backend
                    .restore_execution(&reference.terminal)
                    .await?,
            ))
        })
    }
}
