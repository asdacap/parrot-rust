//! Runs a script's tool calls against the sibling tools of one turn, under the
//! security the codemode call captured. Nested calls are invisible to the turn
//! history: this module writes no events and records nothing.

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::security::{AccessSnapshot, InvocationScope};
use parrot_domain::contracts::tools::ParallelSafety;
use parrot_domain::values::tools::ToolCycleImageBudget;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::agent_turn::services::TurnSelection;
use parrot_runtime::tool_settlement::{
    SharedSecurity, ToolFailure, ToolHandle, ToolInvocation, ToolOutput,
};
use parrot_storage::artifacts::ImageArtifactMetadata;
use std::sync::Arc;

/// One tool call a script makes, addressed by raw tool name.
#[derive(Clone, Debug)]
pub struct NestedCallRequest {
    pub name: String,
    pub arguments_json: String,
}

/// What one nested call produced. `Ok` keeps the tool's image artifacts as
/// data for the result formatter to summarize; the text is script-visible.
#[derive(Clone, Debug)]
pub enum NestedCallOutcome {
    Ok {
        call_id: String,
        text: String,
        image_artifacts: Vec<ImageArtifactMetadata>,
        display_result: Option<String>,
    },
    Failed {
        call_id: String,
        message: String,
    },
}

impl NestedCallOutcome {
    /// The fresh call id, `c<ordinal>` in issue order.
    pub fn call_id(&self) -> &str {
        match self {
            Self::Ok { call_id, .. } | Self::Failed { call_id, .. } => call_id,
        }
    }
}

/// The policy capture one codemode invocation lends to its nested calls.
pub struct NestedToolCapture<S: AccessSnapshot + 'static> {
    /// The codemode call's selection: its model, profile, and security.
    pub selection: TurnSelection<S>,
    pub assistant_sequence: i64,
    pub prompt_templates: Arc<PromptTemplateCatalog>,
    pub image_budget: Option<Arc<ToolCycleImageBudget>>,
    /// The codemode call's cancellation; a cancelled capture cancels every
    /// in-flight nested call.
    pub cancellation: Arc<dyn Cancellation>,
}

impl<S: AccessSnapshot + 'static> Clone for NestedToolCapture<S> {
    fn clone(&self) -> Self {
        Self {
            selection: self.selection.clone(),
            assistant_sequence: self.assistant_sequence,
            prompt_templates: Arc::clone(&self.prompt_templates),
            image_budget: self.image_budget.clone(),
            cancellation: Arc::clone(&self.cancellation),
        }
    }
}

/// Runs nested calls against sibling tools, honoring each tool's parallel
/// safety and returning results in call order. The capture is handed per
/// batch: one codemode invocation lends exactly one capture.
pub trait NestedToolRunner<S: AccessSnapshot + 'static>: Send + Sync {
    fn run(
        &self,
        capture: &NestedToolCapture<S>,
        calls: Vec<NestedCallRequest>,
    ) -> BoxFuture<'_, Vec<NestedCallOutcome>>;
}

/// The production runner over one turn's sibling tool handles. The host
/// injects every tool except codemode itself.
pub struct SiblingToolRunner<S: AccessSnapshot + 'static> {
    tools: Vec<ToolHandle<S>>,
}

fn invocation_for<S: AccessSnapshot + 'static>(
    call_id: &str,
    arguments_json: &str,
    capture: &NestedToolCapture<S>,
) -> ToolInvocation {
    let mut invocation = ToolInvocation::new(
        call_id,
        arguments_json,
        capture.assistant_sequence,
        Arc::clone(&capture.prompt_templates),
    );
    invocation.image_budget = capture.image_budget.clone();
    invocation
}

fn find<'a, S: AccessSnapshot + 'static>(
    tools: &'a [ToolHandle<S>],
    name: &str,
) -> Option<&'a ToolHandle<S>> {
    tools.iter().find(|tool| {
        let metadata = tool.definition();
        metadata.name == name || metadata.aliases.iter().any(|alias| alias == name)
    })
}

/// Borrowed handles cannot be probed for safety; they serialize, which is the
/// conservative classification.
fn handle_safety<S: AccessSnapshot + 'static>(
    tool: &ToolHandle<S>,
    invocation: &ToolInvocation,
) -> ParallelSafety {
    match tool {
        ToolHandle::Ordinary(tool) => tool.parallel_safety(invocation),
        ToolHandle::Borrowed(_) => ParallelSafety::Sequential,
    }
}

/// One call's future: builds the fresh scope and races the tool's work against
/// the captured cancellation, so a cancelled capture aborts in-flight calls
/// even when the tool never polls.
/// Borrowed tools lend turn-owned state, which nested calls never have: they
/// run nothing and fail instead.
fn nested_tool_error() -> OperationError<ToolFailure> {
    OperationError::Failed(ToolFailure::new(
        "borrowed tools cannot run as nested calls",
        parrot_storage::diagnostics::FailureKind::InvalidOperation,
    ))
}

async fn invoke_one<S: AccessSnapshot + 'static>(
    tool: ToolHandle<S>,
    invocation: ToolInvocation,
    capture: NestedToolCapture<S>,
) -> Result<ToolOutput, OperationError<ToolFailure>> {
    let ToolHandle::Ordinary(tool) = tool else {
        return Err(nested_tool_error());
    };
    let selection = capture.selection;
    let answer = tool.invoke(
        invocation,
        InvocationScope::new(
            SharedSecurity(Arc::clone(&selection.security)),
            selection,
            capture.cancellation.as_ref(),
        ),
    );
    let stop = capture.cancellation.cancelled();
    tokio::select! {
        biased;
        _ = stop => Err(OperationError::Cancelled),
        result = answer => result,
    }
}

async fn nested_outcome<S: AccessSnapshot + 'static>(
    tool: Option<ToolHandle<S>>,
    call_id: String,
    request: NestedCallRequest,
    capture: NestedToolCapture<S>,
) -> NestedCallOutcome {
    let Some(tool) = tool else {
        return NestedCallOutcome::Failed {
            call_id,
            message: format!("unknown tool {}", request.name),
        };
    };
    let invocation = invocation_for(&call_id, &request.arguments_json, &capture);
    match invoke_one(tool, invocation, capture).await {
        Ok(output) => NestedCallOutcome::Ok {
            call_id,
            text: output.text,
            image_artifacts: output.image_artifacts,
            display_result: output.display_result,
        },
        Err(OperationError::Failed(failure)) => NestedCallOutcome::Failed {
            call_id,
            message: failure.message,
        },
        Err(OperationError::Cancelled) => NestedCallOutcome::Failed {
            call_id,
            message: "operation cancelled".into(),
        },
    }
}

impl<S: AccessSnapshot + 'static> SiblingToolRunner<S> {
    pub fn new(tools: Vec<ToolHandle<S>>) -> Self {
        Self { tools }
    }
}

impl<S: AccessSnapshot + 'static> NestedToolRunner<S> for SiblingToolRunner<S> {
    fn run(
        &self,
        capture: &NestedToolCapture<S>,
        calls: Vec<NestedCallRequest>,
    ) -> BoxFuture<'_, Vec<NestedCallOutcome>> {
        let capture = capture.clone();
        Box::pin(async move {
            let capture = &capture;
            let mut outcomes = Vec::with_capacity(calls.len());
            // Contiguous parallel-safe calls form one batch; a sequential call
            // runs alone, matching how the turn's batch machinery groups.
            let mut index = 0;
            while index < calls.len() {
                let request = calls[index].clone();
                let tool = find(&self.tools, &request.name);
                let safety = tool
                    .map(|tool| {
                        handle_safety(
                            tool,
                            &invocation_for(
                                &format!("c{}", index + 1),
                                &request.arguments_json,
                                capture,
                            ),
                        )
                    })
                    .unwrap_or(ParallelSafety::Sequential);
                match safety {
                    ParallelSafety::ParallelSafe => {
                        let mut batch = Vec::new();
                        let mut cursor = index;
                        while cursor < calls.len() {
                            let request = calls[cursor].clone();
                            let Some(tool) = find(&self.tools, &request.name) else {
                                break;
                            };
                            let probe = invocation_for(
                                &format!("c{}", cursor + 1),
                                &request.arguments_json,
                                capture,
                            );
                            if handle_safety(tool, &probe) != ParallelSafety::ParallelSafe {
                                break;
                            }
                            batch.push((cursor + 1, tool.clone(), request));
                            cursor += 1;
                        }
                        let mut handles = Vec::with_capacity(batch.len());
                        for (ordinal, tool, request) in batch {
                            let batch_capture = capture.clone();
                            handles.push(tokio::spawn(async move {
                                nested_outcome(
                                    Some(tool),
                                    format!("c{ordinal}"),
                                    request,
                                    batch_capture,
                                )
                                .await
                            }));
                        }
                        for handle in handles {
                            if let Ok(outcome) = handle.await {
                                outcomes.push(outcome);
                            }
                        }
                        index = cursor;
                    }
                    ParallelSafety::Sequential => {
                        outcomes.push(
                            nested_outcome(
                                tool.cloned(),
                                format!("c{}", index + 1),
                                request,
                                capture.clone(),
                            )
                            .await,
                        );
                        index += 1;
                    }
                }
            }
            outcomes
        })
    }
}
