use super::*;
use parrot_domain::{
    contracts::{async_ops::OperationError, security::InvocationContext},
    protocol::messages::parrot::{Delivery, InputAdmitted, MessageContentPart, ToolStarted},
};
use parrot_storage::{
    artifacts::output_blobs::is_tool_output_oversized,
    conversation::ConversationOrigin,
    diagnostics::{DiagnosticFailure, DiagnosticSeverity, FailureKind},
    events::ConversationPart,
};
use std::time::Instant;

impl<S: AccessSnapshot + 'static> RuntimeTools<'_, S> {
    pub(super) fn invoke<'a>(
        &'a self,
        resources: InvocationResources<'a, S>,
        borrowed: Option<TurnResources<'a>>,
        call: &'a ToolCall,
        launched: Option<Arc<AtomicBool>>,
    ) -> BoxFuture<'a, Result<ToolResult, TurnError>> {
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
                .append_message(started.clone(), None, ConversationOrigin::Model)
                .await
                .map_err(TurnError::Storage)?;
            self.publish(started).await?;
            let tool = snapshot.find(&call.name);
            let began = Instant::now();
            self.diagnostic(
                history,
                call,
                tool,
                ToolDiagnostic {
                    operation: "started",
                    severity: DiagnosticSeverity::Information,
                    outcome: None,
                    error_code: None,
                    began: None,
                },
            );
            let Some(tool) = tool else {
                self.diagnostic(
                    history,
                    call,
                    None,
                    ToolDiagnostic {
                        operation: "finished",
                        severity: DiagnosticSeverity::Warning,
                        outcome: Some("unknown_tool"),
                        error_code: None,
                        began: Some(began),
                    },
                );
                return self.fail(call, format!("unknown tool {}", call.name));
            };
            let result: Result<ToolOutput, OperationError<ToolFailure>> = async {
                // Retain the original turn's model/profile. Only security comes
                // from the current capture, and each running call owns its Arc.
                let mut invocation_selection = selection.clone();
                invocation_selection.security = capture.capture().map_err(turn_failure)?.security;
                let mut invocation = ToolInvocation::new(
                    &call.id,
                    &call.arguments_json,
                    assistant_sequence,
                    self.config.templates.clone(),
                );
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
                    settlement.status = ToolStatus::ImageBudgetExceeded;
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
                text = self.config.output_blobs.substitute_process_notice(&text);
                let yielded = result.yielded_process.is_some();
                let metadata = result
                    .yielded_process
                    .as_ref()
                    .map(protobuf::Message::write_to_bytes)
                    .transpose()
                    .map_err(|error| {
                        OperationError::Failed(ToolFailure::new(
                            error.to_string(),
                            FailureKind::Unexpected,
                        ))
                    })?;
                for image in result.image_artifacts {
                    let parts = vec![
                        ConversationPart::text(&image.artifact_id),
                        ConversationPart::image(image),
                    ];
                    let mut event = self.event(history).map_err(turn_failure)?;
                    let admission = history
                        .admit_parts(
                            Identifier::message_id().map_err(|error| {
                                turn_failure(TurnError::Service(error.to_string()))
                            })?,
                            parts,
                            Delivery::DELIVERY_STEER,
                            move |input| {
                                let parts = input
                                    .parts
                                    .iter()
                                    .map(|part| {
                                        let mut part_message = MessageContentPart::new();
                                        match part {
                                            ConversationPart::Text(text) => {
                                                part_message.set_text(text.clone())
                                            }
                                            ConversationPart::ImageArtifact { metadata } => {
                                                part_message
                                                    .set_artifact_id(metadata.artifact_id.clone());
                                                part_message.display_name =
                                                    metadata.display_name.clone();
                                            }
                                        }
                                        part_message
                                    })
                                    .collect();
                                event.set_input_admitted(InputAdmitted {
                                    input_id: input.id.clone(),
                                    message_id: input.message_id.clone(),
                                    content: input.content(),
                                    delivery: input.delivery.into(),
                                    parts,
                                    ..InputAdmitted::new()
                                });
                                Ok(event)
                            },
                        )
                        .await
                        .map_err(TurnError::from)
                        .map_err(turn_failure)?;
                    if let Some(event) = admission.published {
                        self.publish(event).await.map_err(turn_failure)?;
                    }
                }
                self.diagnostic(
                    history,
                    call,
                    Some(tool),
                    ToolDiagnostic {
                        operation: "finished",
                        severity: DiagnosticSeverity::Information,
                        outcome: Some(if yielded { "yielded" } else { "completed" }),
                        error_code: None,
                        began: Some(began),
                    },
                );
                Ok(ToolResult {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    status: ToolStatus::Finished,
                    text,
                    error_message: String::new(),
                    metadata,
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
                        ToolDiagnostic {
                            operation: "finished",
                            severity: DiagnosticSeverity::Information,
                            outcome: Some("cancelled"),
                            error_code: None,
                            began: Some(began),
                        },
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
                        ToolDiagnostic {
                            operation: "finished",
                            severity: DiagnosticSeverity::Error,
                            outcome: Some("failed"),
                            error_code: Some(code),
                            began: Some(began),
                        },
                    );
                    self.fail(call, error.message)
                }
            }
        })
    }
    pub(super) fn cancel(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolStatus::Cancelled,
            text: self.interrupted_result.into(),
            error_message: String::new(),
            metadata: None,
        }
    }
    fn fail(&self, call: &ToolCall, message: String) -> Result<ToolResult, TurnError> {
        let text = self
            .config
            .templates
            .render(
                "tool-result.error",
                &[("message".into(), serde_json::Value::String(message.clone()))],
                &NeverCancelled,
            )
            .map_err(|error| TurnError::Service(error.to_string()))?;
        Ok(ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolStatus::Error,
            text,
            error_message: message,
            metadata: None,
        })
    }
    fn diagnostic(
        &self,
        history: &AgentHistoryView,
        call: &ToolCall,
        tool: Option<&ToolHandle<S>>,
        record: ToolDiagnostic<'_>,
    ) {
        self.config.diagnostics.write(&DiagnosticEvent {
            agent_session_id: Some(history.agent_session_id().into()),
            correlation_id: Some(call.id.clone()),
            tool_name: tool.map(|tool| tool.definition().name.clone()),
            outcome: record.outcome.map(str::to_owned),
            error_code: record.error_code.map(str::to_owned),
            duration_milliseconds: record
                .began
                .map(|start| start.elapsed().as_millis().min(i64::MAX as u128) as i64),
            ..DiagnosticEvent::new("tool", record.operation, record.severity)
        });
    }
}
struct ToolDiagnostic<'a> {
    operation: &'a str,
    severity: DiagnosticSeverity,
    outcome: Option<&'a str>,
    error_code: Option<&'a str>,
    began: Option<Instant>,
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
