use crate::{
    BoxFuture, CancellationToken, ConfigKey, Content, ConversationAddition,
    ConversationPersistence, Error, Message, Role, States, ToolCall, ToolCallReceipt, ToolResult,
    ToolStatus, Tools, budget::TurnFlags, rendering::render_template,
};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct YieldedProcess {
    pub process_id: String,
    pub name: String,
    pub inventory_instance_id: String,
    pub visible_revision: u64,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub activity_kind: i32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolExecutionResult {
    pub result: ToolResult,
    pub yielded_process: Option<YieldedProcess>,
}

impl From<ToolResult> for ToolExecutionResult {
    fn from(result: ToolResult) -> Self {
        Self {
            result,
            yielded_process: None,
        }
    }
}

#[derive(Debug)]
pub struct ToolExecutionRequest {
    pub calls: Vec<ToolCall>,
    pub receipt: ToolCallReceipt,
    pub flags: TurnFlags,
    pub starts_batch: bool,
    pub cancel_remaining: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolRecovery {
    pub through: i64,
    pub batches: Vec<ToolRecoveryBatch>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRecoveryBatch {
    pub sequence: i64,
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResult>,
}

#[derive(Default)]
pub struct ToolSettlement {
    pending: Option<ToolBatch>,
}

struct ToolBatch {
    sequence: i64,
    calls: Vec<ToolCall>,
    results: HashMap<String, ToolResult>,
    index: usize,
}

impl ToolSettlement {
    pub fn reset_pending(&mut self) {
        self.pending = None;
    }

    pub fn begin_execution(&mut self, sequence: i64, calls: Vec<ToolCall>) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::failed("the previous tool batch is unfinished"));
        }
        self.pending = Some(ToolBatch {
            sequence,
            calls,
            results: HashMap::new(),
            index: 0,
        });
        Ok(())
    }

    pub async fn restore_results(&mut self, states: &mut dyn States) -> Result<(), Error> {
        if self.pending.is_some() {
            return Err(Error::failed(
                "cannot restore results during tool execution",
            ));
        }
        let recovery = states.read_tool_recovery().await?;
        for batch in recovery.batches {
            let mut results = batch
                .results
                .into_iter()
                .map(|result| (result.call_id.clone(), result))
                .collect::<HashMap<_, _>>();
            let mut settled = HashMap::new();
            for call in &batch.calls {
                let (result, recovered) = if let Some(result) = results.remove(&call.id) {
                    (result, true)
                } else {
                    let error_message =
                        render_template(states, ConfigKey::CrashedToolTemplate, &[]).await?;
                    let text = render_template(
                        states,
                        ConfigKey::ToolErrorTemplate,
                        &[("message".into(), error_message.clone().into())],
                    )
                    .await?;
                    (
                        ToolResult {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            status: ToolStatus::Error,
                            text,
                            error_message,
                            images: Vec::new(),
                        },
                        false,
                    )
                };
                Self::record_result(
                    states,
                    batch.sequence,
                    &ToolExecutionResult {
                        result: result.clone(),
                        yielded_process: None,
                    },
                    recovered,
                )
                .await?;
                settled.insert(call.id.clone(), result);
            }
            Self::finish_batch(
                states,
                &ToolBatch {
                    sequence: batch.sequence,
                    calls: batch.calls,
                    results: settled,
                    index: 0,
                },
            )
            .await?;
        }
        states.write_tool_watermark(recovery.through).await?;
        Ok(())
    }

    pub async fn record_results(
        &mut self,
        states: &mut dyn States,
        results: &[ToolExecutionResult],
    ) -> Result<(), Error> {
        let mut batch = self
            .pending
            .take()
            .ok_or_else(|| Error::failed("tool execution is not prepared"))?;
        if results.is_empty() {
            return Err(Error::failed("tool execution returned no results"));
        }
        for execution in results {
            let result = &execution.result;
            let call = batch
                .calls
                .get(batch.index)
                .ok_or_else(|| Error::failed("tool execution returned too many results"))?;
            if result.call_id != call.id || result.tool_name != call.name {
                return Err(Error::failed(
                    "tool result does not match the prepared call",
                ));
            }
            Self::record_result(states, batch.sequence, execution, false).await?;
            batch.results.insert(result.call_id.clone(), result.clone());
            batch.index += 1;
        }
        self.pending = Some(batch);
        Ok(())
    }

    pub async fn complete_execution(
        &mut self,
        states: &mut dyn States,
        results: &[ToolResult],
    ) -> Result<(), Error> {
        let batch = self
            .pending
            .take()
            .ok_or_else(|| Error::failed("tool execution is not prepared"))?;
        if batch.index != batch.calls.len()
            || results.len() != batch.calls.len()
            || results
                .iter()
                .zip(&batch.calls)
                .any(|(result, call)| result.call_id != call.id || result.tool_name != call.name)
        {
            return Err(Error::failed(
                "tool execution did not complete the recorded batch",
            ));
        }
        Self::finish_batch(states, &batch).await?;
        states.write_tool_watermark(batch.sequence).await?;
        Ok(())
    }

    fn record_result<'a>(
        states: &'a mut dyn States,
        sequence: i64,
        execution: &'a ToolExecutionResult,
        recovered: bool,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut message = Message::text(Role::Tool, execution.result.text.clone());
            message.tool_call_id = execution.result.call_id.clone();
            states
                .add_conversation(ConversationAddition {
                    messages: vec![message],
                    persistence: ConversationPersistence::ToolResult {
                        assistant_sequence: sequence,
                        result: execution.clone(),
                        recovered,
                    },
                })
                .await?;
            Ok(())
        })
    }

    async fn finish_batch(states: &mut dyn States, batch: &ToolBatch) -> Result<(), Error> {
        let contents = batch
            .calls
            .iter()
            .filter_map(|call| batch.results.get(&call.id))
            .filter(|result| result.status == ToolStatus::Finished)
            .flat_map(|result| result.images.iter().cloned())
            .map(Content::Artifact)
            .collect::<Vec<_>>();
        if !contents.is_empty() {
            states
                .add_conversation(ConversationAddition {
                    messages: vec![Message {
                        role: Role::User,
                        contents,
                        ..Message::default()
                    }],
                    persistence: ConversationPersistence::ToolImages {
                        assistant_sequence: batch.sequence,
                    },
                })
                .await?;
        }
        Ok(())
    }
}

pub async fn execute_tools(
    states: &mut dyn States,
    tools: &dyn Tools,
    calls: Vec<ToolCall>,
    receipt: ToolCallReceipt,
    flags: TurnFlags,
    cancellation: CancellationToken<'_>,
) -> Result<(), Error> {
    let mut settlement = ToolSettlement::default();
    settlement.begin_execution(receipt.assistant_sequence, calls.clone())?;
    let mut results = Vec::with_capacity(calls.len());
    let mut cancel_remaining = false;
    while results.len() < calls.len() {
        let chunk = tools
            .execute(
                ToolExecutionRequest {
                    calls: calls[results.len()..].to_vec(),
                    receipt: ToolCallReceipt {
                        assistant_sequence: receipt.assistant_sequence,
                    },
                    flags,
                    starts_batch: results.is_empty(),
                    cancel_remaining,
                },
                cancellation,
            )
            .await?;
        settlement.record_results(states, &chunk).await?;
        cancel_remaining |= chunk
            .iter()
            .any(|execution| execution.result.status == ToolStatus::Cancelled);
        results.extend(chunk.into_iter().map(|execution| execution.result));
    }
    settlement.complete_execution(states, &results).await
}
