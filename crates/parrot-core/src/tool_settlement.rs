use crate::{
    BoxFuture, CancellationToken, ConfigKey, Content, ConversationAddition,
    ConversationPersistence, Error, Message, Role, States, ToolCall, ToolCallReceipt, ToolResult,
    ToolStatus, Tools, TurnFlags, rendering::render_template,
};
use std::collections::HashMap;

#[derive(Debug)]
pub struct ToolExecutionRequest {
    pub calls: Vec<ToolCall>,
    pub receipt: ToolCallReceipt,
    pub flags: TurnFlags,
    pub starts_batch: bool,
    pub cancel_remaining: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRecovery {
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
        let Some(batch) = states.read_tool_recovery().await? else {
            return Ok(());
        };
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
                        metadata: None,
                    },
                    false,
                )
            };
            Self::record_result(states, batch.sequence, &result, recovered).await?;
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
        Self::finish_batch(states, &batch).await
    }

    fn record_result<'a>(
        states: &'a mut dyn States,
        sequence: i64,
        result: &'a ToolResult,
        recovered: bool,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut message = Message::text(Role::Tool, result.text.clone());
            message
                .contents
                .extend(result.images.iter().cloned().map(Content::Artifact));
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
                .add_conversation_messages(ConversationAddition {
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
            .any(|result| result.status == ToolStatus::Cancelled);
        results.extend(chunk);
    }
    settlement.complete_execution(states, &results).await
}
