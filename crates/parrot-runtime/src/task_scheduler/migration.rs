use super::*;

impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskScheduler<S, P> {
    pub async fn capture_migration(&self) -> TaskGraphMigrationSnapshot {
        self.gate.lock().await.migration.clone()
    }

    /// Captures exact pending notice identities that retirement can safely consume.
    pub async fn capture_migration_notices(&self) -> Vec<(String, String)> {
        let state = self.gate.lock().await;
        let mut notices = state
            .notifications
            .iter()
            .map(|notice| (notice.target_session_id.clone(), notice.message_id.clone()))
            .collect::<Vec<_>>();
        for notice in state
            .entries
            .iter()
            .filter_map(Entry::capture_worker)
            .filter_map(|worker| worker.terminal_notification.as_ref())
            .chain(state.completion_notification.iter())
        {
            notices.push((
                self.config.owner_identity.session.session_id.clone(),
                notice.message_id.clone(),
            ));
        }
        notices
    }

    /// Finds pending input with no exact retirement correlation, without changing it.
    pub async fn find_ambiguous_migration_input(
        &self,
        known_ids: &HashSet<String>,
    ) -> ChildResult<Option<String>> {
        let pending = self
            .config
            .history
            .capture_pending_input_ids()
            .await
            .map_err(|error| ChildError::failure("io", error.to_string()))?;
        Ok(pending.into_iter().find(|id| !known_ids.contains(id)))
    }

    /// Finds graph-owned executions without treating a selected manual predecessor as owned.
    pub async fn capture_owned_workers(&self) -> ChildResult<Vec<String>> {
        let state = self.gate.lock().await;
        self.ensure_healthy(&state)?;
        let mut owned = Vec::new();
        for entry in &state.entries {
            let Some(worker) = entry.capture_worker() else {
                continue;
            };
            if !matches!(
                entry.task.state,
                ExecutionStatus::Pending | ExecutionStatus::Running
            ) && worker.pending_stop.is_none()
            {
                continue;
            }
            let prompt_id = worker
                .pending_stop
                .as_ref()
                .map(|stop| &stop.prompt_message_id)
                .or_else(|| {
                    worker
                        .runner
                        .as_ref()
                        .map(|runner| &runner.prompt_message_id)
                });
            let (Some(prompt_id), Some(scope)) = (prompt_id, &worker.scope) else {
                continue;
            };
            let session = scope.get_service::<dyn TaskSession>()?;
            let Some(saved) = session.capture_execution().await? else {
                continue;
            };
            if saved.initial_message_id != *prompt_id {
                continue;
            }
            let claim = TaskResultClaim {
                owner_session_id: self.config.owner_identity.session.session_id.clone(),
                claim_id: prompt_id.clone(),
            };
            if saved
                .result_claim
                .as_ref()
                .is_some_and(|saved| saved != &claim)
                || (saved.terminal.is_none() && saved.result_claim.as_ref() != Some(&claim))
            {
                return Err(ChildError::registry(
                    "legacy task execution has no matching protected claim",
                ));
            }
            owned.push(scope.identity().session.session_id.clone());
        }
        owned.sort();
        owned.dedup();
        Ok(owned)
    }

    /// Saves retirement ownership before any old execution is stopped or detached.
    pub async fn prepare_migration(
        &self,
        disposition: TaskMigrationDisposition,
        mut excluded_session_ids: Vec<String>,
    ) -> ChildResult<()> {
        self.admitted().await;
        let mut state = self.gate.lock().await;
        self.ensure_healthy(&state)?;
        if state.migration == TaskGraphMigrationSnapshot::Current
            && disposition == TaskMigrationDisposition::Reassign
        {
            return Ok(());
        }
        if let TaskGraphMigrationSnapshot::Retiring {
            disposition: saved,
            excluded_session_ids: excluded,
        } = &state.migration
        {
            if *saved != disposition {
                return Err(ChildError::registry(
                    "legacy migration disposition changed during recovery",
                ));
            }
            excluded_session_ids.extend(excluded.iter().cloned());
        }
        excluded_session_ids.sort();
        excluded_session_ids.dedup();
        state.execution_active = false;
        state.notifications_active = false;
        state.migration = TaskGraphMigrationSnapshot::Retiring {
            disposition,
            excluded_session_ids,
        };
        for entry in &mut state.entries {
            let execution = std::mem::replace(&mut entry.execution, TaskExecution::Unassigned);
            entry.execution = match execution {
                TaskExecution::Dedicated(worker)
                    if worker.pending_stop.is_some()
                        || (matches!(
                            entry.task.state,
                            ExecutionStatus::Pending | ExecutionStatus::Running
                        ) && worker.runner.is_some()) =>
                {
                    TaskExecution::RetiringLegacy(worker)
                }
                execution => execution,
            };
            if disposition == TaskMigrationDisposition::CancelObsolete
                && matches!(
                    entry.task.state,
                    ExecutionStatus::Pending | ExecutionStatus::Running
                )
            {
                entry.task.state = ExecutionStatus::Canceled;
            } else if matches!(entry.execution, TaskExecution::RetiringLegacy(_))
                && entry.task.state == ExecutionStatus::Running
            {
                entry.task.state = ExecutionStatus::Pending;
            }
        }
        for index in 0..state.entries.len() {
            if matches!(
                state.entries[index].execution,
                TaskExecution::RetiringLegacy(_)
            ) && state.entries[index].borrow_worker().pending_stop.is_none()
                && state.entries[index].borrow_worker().runner.is_some()
            {
                self.stop_run(&mut state, index, "agent-task.stop-restarted")?;
            }
        }
        self.save_graph(&mut state).await
    }

    /// Joins complete cancellation and claim release before making new assignments eligible.
    pub async fn retire_legacy(self: &Arc<Self>) -> ChildResult<()> {
        let executions = {
            let mut state = self.gate.lock().await;
            self.ensure_healthy(&state)?;
            match state.migration {
                TaskGraphMigrationSnapshot::Current => return Ok(()),
                TaskGraphMigrationSnapshot::Pending => {
                    return Err(ChildError::registry(
                        "legacy migration must be prepared before retirement",
                    ));
                }
                TaskGraphMigrationSnapshot::Retiring { .. } => {}
            }
            self.kick_stops(&mut state)?;
            state.executions.clone()
        };
        let mut failure = None;
        for execution in executions {
            if let Err(error) = execution.join().await {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let obsolete_notices = {
            let state = self.gate.lock().await;
            self.ensure_healthy(&state)?;
            match state.migration {
                TaskGraphMigrationSnapshot::Retiring {
                    disposition: TaskMigrationDisposition::CancelObsolete,
                    ..
                } => state
                    .notifications
                    .iter()
                    .map(|notice| {
                        let session = if notice.target_session_id
                            == self.config.owner_identity.session.session_id
                        {
                            self.config.owner_session.clone()
                        } else {
                            state
                                .entries
                                .iter()
                                .filter_map(Entry::capture_worker)
                                .filter_map(|worker| worker.scope.as_ref())
                                .find(|scope| {
                                    scope.identity().session.session_id == notice.target_session_id
                                })
                                .ok_or_else(|| {
                                    ChildError::registry(
                                        "legacy notice target is not a retained task worker",
                                    )
                                })?
                                .get_service::<dyn TaskSession>()?
                        };
                        Ok((notice.message_id.clone(), session))
                    })
                    .chain(
                        state
                            .entries
                            .iter()
                            .filter_map(Entry::capture_worker)
                            .filter_map(|worker| worker.terminal_notification.as_ref())
                            .chain(state.completion_notification.iter())
                            .map(|notice| {
                                Ok((notice.message_id.clone(), self.config.owner_session.clone()))
                            }),
                    )
                    .collect::<ChildResult<Vec<_>>>()?,
                _ => Vec::new(),
            }
        };
        for (message_id, session) in obsolete_notices {
            session.cancel_notification(&message_id).await?;
        }
        let mut state = self.gate.lock().await;
        self.ensure_healthy(&state)?;
        let disposition = match state.migration {
            TaskGraphMigrationSnapshot::Current => return Ok(()),
            TaskGraphMigrationSnapshot::Retiring { disposition, .. } => disposition,
            TaskGraphMigrationSnapshot::Pending => {
                unreachable!("prepared migration remains retiring")
            }
        };
        for entry in &mut state.entries {
            let execution = std::mem::replace(&mut entry.execution, TaskExecution::Unassigned);
            entry.execution = match execution {
                TaskExecution::RetiringLegacy(worker) => {
                    if worker.pending_stop.is_some() {
                        return Err(ChildError::registry(
                            "legacy stop is unfinished after its operation joined",
                        ));
                    }
                    if matches!(
                        entry.task.state,
                        ExecutionStatus::Pending | ExecutionStatus::Running
                    ) {
                        entry.task.state = ExecutionStatus::Pending;
                        entry.task.result = None;
                        entry.task.failure = None;
                        TaskExecution::Unassigned
                    } else {
                        TaskExecution::Dedicated(worker)
                    }
                }
                execution => execution,
            };
        }
        if disposition == TaskMigrationDisposition::CancelObsolete {
            state.notifications.clear();
            state.completion_notification = None;
            state.complete = resolved(&state);
        }
        state.executions.clear();
        state.migration = TaskGraphMigrationSnapshot::Current;
        self.publish(&mut state).await
    }
}
