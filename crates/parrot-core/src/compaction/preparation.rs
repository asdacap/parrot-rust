use super::{
    CompactionConfiguration, CompactionHistoryGroup, CompactionInput, CompactionPreparationEffects,
    ConversationHistoryItem, ConversationState, StatusRenderer,
};
use crate::{CancellationToken, Content, ContextSnapshot, Message, Role, ToolDefinition};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
};

/// Samples instructions and assembles compaction input from one raw conversation snapshot.
/// Preparation errors publish failure after a successful start, before returning.
pub async fn prepare_compaction<E, R>(
    raw: R,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    preparation: &dyn CompactionPreparationEffects<Error = E>,
    status: &dyn StatusRenderer<Error = E>,
    estimate_context: impl Fn(&str, &[ToolDefinition], &[Message]) -> Result<i64, E>,
    cancellation: CancellationToken<'_>,
) -> Result<CompactionInput, E>
where
    E: fmt::Display + Send,
    R: Future<Output = Result<ConversationState, E>>,
{
    preparation.report_started().await?;
    let outcome: Result<CompactionInput, E> = async {
        preparation.sample_epoch(cancellation).await?;
        let instructions = preparation.render_instructions(cancellation).await?;
        let raw = raw.await?;
        let base_watermark = raw.summary.as_ref().map_or(0, |summary| summary.watermark);
        let groups = assemble_groups(&raw);
        let estimate = estimate_context(&instructions, definitions, &raw.messages)?;
        let context = ContextSnapshot::estimate(estimate, &configuration.context);
        let status = status.render(context, cancellation).await?;
        Ok(CompactionInput {
            groups,
            base_watermark,
            instructions,
            messages: raw.messages,
            fixed_message: Message {
                role: Role::System,
                contents: vec![Content::Text(status)],
                ..Message::default()
            },
        })
    }
    .await;
    match outcome {
        Ok(input) => Ok(input),
        Err(error) => {
            preparation.report_failed(error.to_string()).await?;
            Err(error)
        }
    }
}

fn assemble_groups(raw: &ConversationState) -> Vec<CompactionHistoryGroup> {
    let checkpoints: BTreeSet<_> = raw.checkpoint_sequences.iter().copied().collect();
    let mut children: BTreeMap<i64, Vec<&ConversationHistoryItem>> = BTreeMap::new();
    for item in &raw.history {
        if item.is_compaction_status {
            continue;
        }
        if let Some(owner) = item.tool_owner_sequence {
            children.entry(owner).or_default().push(item);
        }
    }
    let mut groups = Vec::new();
    if let Some(summary) = &raw.summary {
        groups.push(CompactionHistoryGroup {
            messages: vec![Message {
                role: Role::System,
                contents: vec![Content::Text(summary.content.clone())],
                ..Message::default()
            }],
            end_watermark: summary.watermark,
            has_checkpoint_before: false,
            is_complete: true,
        });
    }
    for item in &raw.history {
        if item.is_compaction_status || item.tool_owner_sequence.is_some() {
            continue;
        }
        let mut members = vec![item];
        members.extend(children.remove(&item.sequence).into_iter().flatten());
        members.sort_by_key(|member| member.sequence);
        let has_calls = item.message.role == Role::Assistant && !item.message.tool_calls.is_empty();
        let is_complete = !has_calls
            || item.message.tool_calls.iter().all(|call| {
                members.iter().any(|member| {
                    member.message.role == Role::Tool && member.message.tool_call_id == call.id
                })
            });
        groups.push(CompactionHistoryGroup {
            end_watermark: members.last().expect("group has its owner").sequence,
            messages: members
                .into_iter()
                .map(|member| member.message.clone())
                .collect(),
            has_checkpoint_before: has_calls
                && item.sequence > 0
                && checkpoints.contains(&item.sequence),
            is_complete,
        });
    }
    groups
}
