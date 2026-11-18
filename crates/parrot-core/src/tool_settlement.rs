use crate::{
    BoxFuture, CancellationToken, ConfigKey, ConversationAddition, ConversationPersistence, Error,
    Message, Role, States, ToolCall, ToolCallReceipt, ToolExecutionRequest, ToolResult,
    ToolSettlement, ToolStatus, Tools, TurnFlags, rendering::render_template,
};
use std::collections::HashMap;

pub(crate) struct ToolBatch {
    sequence: i64,
    calls: Vec<ToolCall>,
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
        let Some(batch) = states.read_tool_recovery().await? else {
            return Ok(());
        };
        let mut results = batch
            .results
            .into_iter()
            .map(|result| (result.call_id.clone(), result))
            .collect::<HashMap<_, _>>();
        for call in &batch.calls {
            let (result, recovered) = if let Some(result) = results.remove(&call.id) {
                (result, true)
            } else {
                let error_message =
                    render_template(states, ConfigKey::CrashedToolTemplate, &[]).await?;
                let result_text = render_template(
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
                        text: result_text,
                        error_message,
                        display_result: None,
                    },
                    false,
                )
            };
            Self::record_result(states, batch.sequence, &result, recovered).await?;
        }
        Ok(())
    }

    pub async fn record_results(
        &mut self,
        states: &mut dyn States,
        results: &[ToolResult],
    ) -> Result<(), Error> {
        let mut batch = self
            .pending
            .take()
            .ok_or_else(|| Error::failed("tool execution is not prepared"))?;
        if results.is_empty() {
            return Err(Error::failed("tool execution returned no results"));
        }
        for result in results {
            let call = batch
                .calls
                .get(batch.index)
                .ok_or_else(|| Error::failed("tool execution returned too many results"))?;
            if result.call_id != call.id || result.tool_name != call.name {
                return Err(Error::failed(
                    "tool result does not match the prepared call",
                ));
            }
            Self::record_result(states, batch.sequence, result, false).await?;
            batch.index += 1;
        }
        self.pending = Some(batch);
        Ok(())
    }

    pub fn complete_execution(&mut self, results: &[ToolResult]) -> Result<(), Error> {
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
        Ok(())
    }

    fn record_result<'a>(
        states: &'a mut dyn States,
        sequence: i64,
        result: &'a ToolResult,
        recovered: bool,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut message = Message::text(Role::Tool, result.text.clone());
            message.tool_call_id = result.call_id.clone();
            states
                .add_conversation_messages(ConversationAddition {
                    messages: vec![message],
                    persistence: ConversationPersistence::ToolResult {
                        assistant_sequence: sequence,
                        result: result.clone(),
                        recovered,
                    },
                })
                .await?;
            Ok(())
        })
    }
}

/// Settles the full tool batch and returns its first successful compaction request
/// with the results the batch recorded.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SettledTools {
    pub(crate) compaction_request: Option<crate::CompactionRequest>,
    pub(crate) results: Vec<ToolResult>,
}

pub(crate) async fn execute_tools(
    states: &mut dyn States,
    tools: &dyn Tools,
    calls: Vec<ToolCall>,
    receipt: ToolCallReceipt,
    flags: TurnFlags,
    cancellation: CancellationToken<'_>,
) -> Result<SettledTools, Error> {
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
            .any(|result| result.status == ToolStatus::Cancelled);
        results.extend(chunk);
    }
    settlement.complete_execution(&results)?;
    Ok(SettledTools {
        compaction_request: calls.iter().zip(&results).find_map(|(call, result)| {
            if result.requests_compaction() {
                crate::CompactionRequest::parse(&call.arguments_json).ok()
            } else {
                None
            }
        }),
        results,
    })
}

#[cfg(test)]
mod tests;
