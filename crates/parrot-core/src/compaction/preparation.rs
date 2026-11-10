use super::{
    CompactionConfiguration, CompactionHistoryGroup, CompactionInput, ConversationHistoryItem,
    ConversationState,
};
use crate::{
    CancellationToken, Content, ContextSnapshot, Error, EventPublisher, Message, Provider, Role,
    States, ToolDefinition,
};
use std::collections::{BTreeMap, BTreeSet};

/// Samples instructions and assembles compaction input from raw conversation reads.
/// Preparation errors publish failure after a successful start, before returning.
pub async fn prepare_compaction(
    states: &mut dyn States,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    cancellation: CancellationToken<'_>,
) -> Result<CompactionInput, Error> {
    publisher.report_compaction_started().await?;
    let outcome: Result<CompactionInput, Error> = async {
        publisher.on_epoch_started().await?;
        let instructions = states.capture_system_prompt().await?;
        let raw = crate::read_conversation_state(states).await?;
        let base_watermark = raw.summary.as_ref().map_or(0, |summary| summary.watermark);
        let groups = assemble_groups(&raw);
        let estimate = provider.estimate_input_tokens(&instructions, definitions, &raw.messages)?;
        let context = ContextSnapshot::estimate(estimate, &configuration.context);
        let status = states.render_status(context, cancellation).await?;
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
            publisher
                .report_compaction_failed(error.to_string())
                .await?;
            Err(error)
        }
    }
}

pub(crate) fn assemble_groups(raw: &ConversationState) -> Vec<CompactionHistoryGroup> {
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
