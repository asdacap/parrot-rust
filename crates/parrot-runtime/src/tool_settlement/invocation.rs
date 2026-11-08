use super::*;
use parrot_domain::{
    contracts::{async_ops::OperationError, security::InvocationContext},
    protocol::messages::parrot::ToolStarted,
};
use parrot_storage::{
    artifacts::output_blobs::is_tool_output_oversized,
    diagnostics::{DiagnosticFailure, DiagnosticSeverity, FailureKind},
};
use std::time::Instant;

impl<S: AccessSnapshot + 'static> ToolSettlement<S> {
    pub(super) fn invoke<'a>(
        &'a self,
        resources: InvocationResources<'a, S>,
        borrowed: Option<TurnResources<'a>>,
        call: &'a ToolCall,
        launched: Option<Arc<AtomicBool>>,
    ) -> BoxFuture<'a, Result<ToolExecutionResult, TurnError>> {
        let InvocationResources {
            history,
            usage,
            selection,
            snapshot,
            capture,
            assistant_sequence,
            image_budget,
            cancellation,
        } = resources;
        Box::pin(async move {
            // The durable start/publication boundary is deliberately outside the
            // tool catch boundary, before policy capture, usage and activity.
            let mut started = self.event(history)?;
            started.set_tool_started(ToolStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                path_variables: self.config.path_variables.clone().into_iter().collect(),
                ..ToolStarted::new()
            });
            history
                .append(started.clone(), None, None)
                .await
                .map_err(TurnError::Storage)?;
            self.publish(started).await?;
            let tool = snapshot.find(&call.name);
            let began = Instant::now();
            self.diagnostic(
                history,
                call,
                tool,
                "started",
                DiagnosticSeverity::Information,
                None,
                None,
                None,
            );
            let Some(tool) = tool else {
                self.diagnostic(
                    history,
                    call,
                    None,
                    "finished",
                    DiagnosticSeverity::Warning,
                    Some("unknown_tool"),
                    None,
                    Some(began),
                );
                return self.fail(call, format!("unknown tool {}", call.name));
            };
            let result: Result<ToolOutput, OperationError<ToolFailure>> = async {
                // Retain the original turn's model/profile. Only security comes
                // from the current capture, and each running call owns its Arc.
                let mut invocation_selection = selection.clone();
                invocation_selection.security = capture.capture().map_err(turn_failure)?.security;
                let mut invocation =
                    ToolInvocation::new(&call.id, &call.arguments_json, assistant_sequence);
                invocation.prompt_templates = Some(self.config.templates.clone());
                invocation.image_budget = Some(image_budget);
                let activity = ActiveTool::new(&self.config.activity, &tool.definition().name);
                usage
                    .record_tool(
                        assistant_sequence,
                        &sessions::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments_json: call.arguments_json.clone(),
                        },
                    )
                    .await
                    .map_err(|error| {
                        OperationError::Failed(ToolFailure::new(
                            error.to_string(),
                            FailureKind::InvalidOperation,
                        ))
                    })?;
                if let Some(launched) = launched {
                    launched.store(true, Ordering::Release);
                }
                let result = match tool {
                    ToolHandle::Ordinary(tool) => {
                        let security = SharedSecurity(invocation_selection.security.clone());
                        tool.invoke(
                            invocation,
                            InvocationContext::new(security, invocation_selection, cancellation),
                        )
                        .await
                    }
                    ToolHandle::Borrowed(tool) => {
                        let mut resources = borrowed.ok_or_else(|| {
                            OperationError::Failed(ToolFailure::new(
                                "A borrowed tool requires in-drain resources.",
                                FailureKind::InvalidOperation,
                            ))
                        })?;
                        tool.invoke(
                            invocation,
                            BorrowedToolContext {
                                selection: &invocation_selection,
                                resources: reborrow(&mut resources),
                                definitions: snapshot.definitions(),
                            },
                            cancellation,
                        )
                        .await
                    }
                };
                drop(activity);
                result
            }
            .await;
            // Formatting and output persistence share the source tool failure
            // boundary; a cancelled output is not normalized to a success.
            let rendered = async {
                let result = result?;
                if result.outcome == ToolOutcome::ImageBudgetExceeded {
                    let mut settlement = self.fail(call, result.text).map_err(turn_failure)?;
                    settlement.result.status = ToolStatus::ImageBudgetExceeded;
                    return Ok(settlement);
                }
                let mut text = self
                    .config
                    .templates
                    .render(
                        "tool-result.text",
                        &[("value".into(), serde_json::Value::String(result.text))],
                        &NeverCancelled,
                    )
                    .map_err(|error| {
                        OperationError::Failed(ToolFailure::new(
                            error.to_string(),
                            FailureKind::Argument,
                        ))
                    })?;
                if is_tool_output_oversized(&text) {
                    let store = self.config.output_blobs.clone();
                    text =
                        tokio::task::spawn_blocking(move || store.persist(&text, &NeverCancelled))
                            .await
                            .map_err(|error| {
                                OperationError::Failed(ToolFailure::new(
                                    error.to_string(),
                                    FailureKind::Unexpected,
                                ))
                            })?
                            .map_err(|error| {
                                OperationError::Failed(ToolFailure::new(
                                    error.to_string(),
                                    FailureKind::Io,
                                ))
                            })?;
                }
                let yielded = result.yielded_process.is_some();
                let images = result
                    .image_artifacts
                    .into_iter()
                    .map(|image| ToolImage {
                        artifact_id: image.artifact_id,
                        sha256: image.sha256,
                        media_type: image.media_type,
                        byte_length: image.byte_length,
                        width: image.width,
                        height: image.height,
                        frame_count: image.frame_count,
                        aggregate_pixels: image.aggregate_pixels,
                        display_name: image.display_name,
                        origin: image.origin,
                    })
                    .collect();
                let yielded_process = result.yielded_process;
                self.diagnostic(
                    history,
                    call,
                    Some(tool),
                    "finished",
                    DiagnosticSeverity::Information,
                    Some(if yielded { "yielded" } else { "completed" }),
                    None,
                    Some(began),
                );
                Ok(ToolExecutionResult {
                    result: ToolResult {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        status: ToolStatus::Finished,
                        text,
                        error_message: String::new(),
                        images,
                    },
                    yielded_process,
                })
            }
            .await;
            match rendered {
                Ok(settlement) => Ok(settlement),
                Err(OperationError::Cancelled) => {
                    self.diagnostic(
                        history,
                        call,
                        Some(tool),
                        "finished",
                        DiagnosticSeverity::Information,
                        Some("cancelled"),
                        None,
                        Some(began),
                    );
                    Ok(self.cancel(call))
                }
                Err(OperationError::Failed(error)) => {
                    let code = DiagnosticEvent::classify_failure(&DiagnosticFailure {
                        kind: error.kind,
                        message: "",
                        path: None,
                    });
                    self.diagnostic(
                        history,
                        call,
                        Some(tool),
                        "finished",
                        DiagnosticSeverity::Error,
                        Some("failed"),
                        Some(code),
                        Some(began),
                    );
                    self.fail(call, error.message)
                }
            }
        })
    }
    pub(super) fn cancel(&self, call: &ToolCall) -> ToolExecutionResult {
        ToolExecutionResult {
            result: ToolResult {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                status: ToolStatus::Cancelled,
                text: self.interrupted_result.clone(),
                error_message: String::new(),
                images: Vec::new(),
            },
            yielded_process: None,
        }
    }
    fn fail(&self, call: &ToolCall, message: String) -> Result<ToolExecutionResult, TurnError> {
        let text = self
            .config
            .templates
            .render(
                "tool-result.error",
                &[("message".into(), serde_json::Value::String(message.clone()))],
                &NeverCancelled,
            )
            .map_err(|error| TurnError::Service(error.to_string()))?;
        Ok(ToolExecutionResult {
            result: ToolResult {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                status: ToolStatus::Error,
                text,
                error_message: message,
                images: Vec::new(),
            },
            yielded_process: None,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn diagnostic(
        &self,
        history: &AgentHistoryView,
        call: &ToolCall,
        tool: Option<&ToolHandle<S>>,
        operation: &str,
        severity: DiagnosticSeverity,
        outcome: Option<&str>,
        error_code: Option<&str>,
        began: Option<Instant>,
    ) {
        self.config.diagnostics.write(&DiagnosticEvent {
            agent_session_id: Some(history.agent_session_id().into()),
            correlation_id: Some(call.id.clone()),
            tool_name: tool.map(|tool| tool.definition().name.clone()),
            outcome: outcome.map(str::to_owned),
            error_code: error_code.map(str::to_owned),
            duration_milliseconds: began
                .map(|start| start.elapsed().as_millis().min(i64::MAX as u128) as i64),
            ..DiagnosticEvent::new("tool", operation, severity)
        });
    }
}
struct ActiveTool<'a> {
    activity: &'a AgentSessionActivity,
    execution: i64,
}
impl<'a> ActiveTool<'a> {
    fn new(activity: &'a AgentSessionActivity, name: &str) -> Self {
        Self {
            activity,
            execution: activity.begin_tool(name),
        }
    }
}
impl Drop for ActiveTool<'_> {
    fn drop(&mut self) {
        self.activity.finish_tool(self.execution);
    }
}
fn turn_failure(error: TurnError) -> OperationError<ToolFailure> {
    if error.is_cancelled() {
        OperationError::Cancelled
    } else {
        OperationError::Failed(ToolFailure::new(
            error.to_string(),
            FailureKind::InvalidOperation,
        ))
    }
}
