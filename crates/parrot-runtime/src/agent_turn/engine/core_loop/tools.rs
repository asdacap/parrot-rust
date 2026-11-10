use super::*;

impl<S, T> parrot_core::Tools for RuntimeTools<'_, S, T>
where
    S: AccessSnapshot + 'static,
    T: TurnTools<S> + 'static,
    T::Snapshot: 'static,
{
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, CoreError>> {
        adapt_operation(async move {
            let selection = self.binding.capture_selection()?;
            let normal = self.backend.snapshot(&selection)?;
            let final_phase = self.backend.tools.enabled_after_interruption(&normal);
            let catalog = ToolCatalog {
                normal: self
                    .backend
                    .tools
                    .definitions(&normal)
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
                final_phase: self
                    .backend
                    .tools
                    .definitions(&final_phase)
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
            };
            *self.captured.lock().expect("captured tools poisoned") = Some(CapturedTools {
                normal,
                final_phase,
            });
            Ok(catalog)
        })
    }
    fn execute<'a>(
        &'a self,
        request: parrot_core::ToolExecutionRequest,
        cancellation: parrot_core::CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<parrot_core::ToolExecutionResult>, CoreError>> {
        adapt_operation(async move {
            let backend = self.backend;
            let selection = self.binding.capture_selection()?;
            let snapshot = {
                let guard = self.captured.lock().expect("captured tools poisoned");
                let tools = guard
                    .as_ref()
                    .ok_or_else(|| TurnError::Service("turn tools are unavailable".into()))?;
                if request.flags.final_phase {
                    tools.final_phase.clone()
                } else {
                    tools.normal.clone()
                }
            };
            let mut guard = backend.state.lock().await;
            let state = &mut *guard;
            let status = state
                .request_status
                .clone()
                .ok_or_else(|| TurnError::Service("request status is unavailable".into()))?;
            let events = ContextEmitter::new(
                &backend.history,
                backend.publisher.as_ref(),
                &backend.broker,
                backend.event_id.as_ref(),
            );
            let chunk = backend
                .tools
                .execute(
                    ToolExecutionChunk {
                        assistant_sequence: request.receipt.assistant_sequence,
                        starts_batch: request.starts_batch,
                        calls: request.calls,
                        cancel_remaining: request.cancel_remaining,
                    },
                    TurnResources {
                        history: &backend.history,
                        conversation: &mut state.conversation,
                        context: &mut state.context,
                        prompt: &backend.prompt,
                        sessions: &backend.sessions,
                        status: status.as_ref(),
                        events: &events,
                        usage: &backend.usage,
                    },
                    &selection,
                    &snapshot,
                    backend.selection.as_ref(),
                    &cancellation,
                )
                .await?;
            Ok(chunk
                .into_iter()
                .map(|execution| parrot_core::ToolExecutionResult {
                    result: execution.result,
                    yielded_process: execution.yielded_process.map(|value| {
                        parrot_core::YieldedProcess {
                            process_id: value.process_id,
                            name: value.name,
                            inventory_instance_id: value.inventory_instance_id,
                            visible_revision: value.visible_revision,
                            stdout_path: value.stdout_path,
                            stderr_path: value.stderr_path,
                            activity_kind: value.activity_kind.value(),
                        }
                    }),
                })
                .collect())
        })
    }
}
