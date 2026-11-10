use super::*;

impl<S: RestrictableAccessSnapshot + 'static, P: TaskArtifactParser + 'static> TaskScheduler<S, P> {
    pub(super) fn has_preparing(state: &State<S>) -> bool {
        state
            .accepted_calls
            .iter()
            .any(|call| matches!(call.receipt, TaskCallReceipt::Preparing { .. }))
    }

    pub(super) fn find_receipt<'a>(
        state: &'a State<S>,
        identity: &TaskCallIdentity,
    ) -> ChildResult<Option<&'a TaskCallReceipt>> {
        if state.accepted_calls.first().is_some_and(|receipt| {
            identity.assistant_sequence < receipt.identity.assistant_sequence
        }) {
            return Err(ChildError::registry(
                "task update call belongs to an older settled assistant batch",
            ));
        }
        Ok(state
            .accepted_calls
            .iter()
            .find(|call| call.identity == *identity)
            .map(|call| &call.receipt))
    }

    pub(super) fn restore_response(
        &self,
        receipt: &TaskCallReceipt,
    ) -> ChildResult<TaskSetResponse> {
        let (count, text) = match receipt {
            TaskCallReceipt::LegacyCount { accepted_count } => (
                *accepted_count,
                render(
                    &self.config.configuration,
                    "agent-task.set-accepted",
                    &[("count", &accepted_count.to_string())],
                )?,
            ),
            TaskCallReceipt::Accepted {
                accepted_count,
                response_text,
            } => (*accepted_count, response_text.clone()),
            TaskCallReceipt::Preparing { .. } => {
                return Err(ChildError::registry("accepted task response is not sealed"));
            }
        };
        Ok(TaskSetResponse {
            accepted_count: usize::try_from(count)
                .map_err(|_| ChildError::registry("accepted task count exceeds this platform"))?,
            text,
        })
    }

    pub(super) fn render_response(&self, state: &State<S>, count: u64) -> ChildResult<String> {
        let self_assignment = state
            .entries
            .iter()
            .find(|entry| entry.owns_self())
            .map(|entry| {
                let task = &entry.task;
                let TaskPayload::Instruction(instruction) = &task.payload else {
                    unreachable!("self assignments contain an instruction");
                };
                let dependencies = task
                    .dependencies
                    .iter()
                    .map(|name| {
                        let dependency =
                            &state.entries[find(state, name).expect("validated dependency")].task;
                        serde_json::json!({
                            "name": dependency.name,
                            "description": dependency.description,
                            "result": dependency.result.as_deref().unwrap_or(""),
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "name": task.name,
                    "description": task.description,
                    "instruction": instruction,
                    "acceptance_criteria": task.acceptance_criteria,
                    "dependencies": dependencies,
                })
            });
        let dedicated = state
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if effective(state, index) != ExecutionStatus::Running {
                    return None;
                }
                let Some(TaskAssignee::Dedicated { agent_path }) = entry.capture_assignee() else {
                    return None;
                };
                Some(serde_json::json!({
                    "name": entry.task.name,
                    "description": entry.task.description,
                    "agent_path": agent_path,
                }))
            })
            .collect::<Vec<_>>();
        self.config
            .configuration
            .templates
            .render(
                "agent-task.set-assigned",
                &[
                    ("count".into(), count.to_string().into()),
                    ("self_assignment".into(), self_assignment.into()),
                    ("dedicated".into(), dedicated.into()),
                ],
                &NeverCancelled,
            )
            .map_err(|error| ChildError::failure("unexpected", error.to_string()))
    }

    pub(super) async fn seal_preparing(self: &Arc<Self>, state: &mut State<S>) -> ChildResult<()> {
        let Some(index) = state
            .accepted_calls
            .iter()
            .position(|call| matches!(call.receipt, TaskCallReceipt::Preparing { .. }))
        else {
            return Ok(());
        };
        self.ensure_healthy(state)?;
        if state.suspended || state.settling {
            return Err(shutting_down());
        }
        if !state.recovery_prepared {
            return Err(ChildError::registry(
                "task recovery must be prepared before sealing an accepted response",
            ));
        }
        self.reconcile(state).await?;
        let TaskCallReceipt::Preparing { accepted_count } = state.accepted_calls[index].receipt
        else {
            unreachable!("receipt remains preparing under the graph gate");
        };
        let text = self.render_response(state, accepted_count)?;
        let current_self = state
            .entries
            .iter()
            .find(|entry| entry.owns_self())
            .and_then(|entry| {
                if let TaskExecution::Owner { generation_id } = &entry.execution {
                    Some((entry.task.name.clone(), generation_id.clone()))
                } else {
                    None
                }
            });
        if let Some((name, generation)) = current_self {
            state.notifications.retain(|notice| {
                !notice.owner_assignment.as_ref().is_some_and(|assignment| {
                    assignment.task_name == name && assignment.generation_id == generation
                })
            });
        }
        state.accepted_calls[index].receipt = TaskCallReceipt::Accepted {
            accepted_count,
            response_text: text,
        };
        self.save_graph(state).await
    }

    /// Seals accepted calls before ordinary recovery can change their assignment view.
    pub async fn finalize_preparing(self: &Arc<Self>) -> ChildResult<()> {
        self.admitted().await;
        let mut state = self.gate.lock().await;
        let preparing = Self::has_preparing(&state);
        self.seal_preparing(&mut state).await?;
        if preparing {
            self.publish(&mut state).await?;
        }
        Ok(())
    }
}
