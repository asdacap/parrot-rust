use crate::{
    CompactionConfiguration, CompactionHistoryGroup, CompactionInput, Content, ContextSnapshot,
    ConversationHistoryItem, ConversationState, Error, EventPublisher, HistoryGroupMember, Message,
    Provider, Role, States, ToolDefinition, assemble_history_groups, join_status_offering,
};
use std::collections::BTreeMap;

/// Samples instructions and assembles compaction input from raw conversation reads.
/// Preparation errors publish failure after a successful start, before returning.
pub async fn prepare_compaction(
    states: &mut dyn States,
    provider: &dyn Provider,
    publisher: &dyn EventPublisher,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
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
        let offering = states.read_status_offering(context).await?;
        Ok(CompactionInput {
            groups,
            base_watermark,
            instructions,
            messages: raw.messages,
            fixed_message: Message {
                role: Role::System,
                contents: vec![Content::Text(join_status_offering(&offering))],
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
    let members: Vec<HistoryGroupMember> = raw.history.iter().map(history_group_member).collect();
    let items: BTreeMap<i64, &ConversationHistoryItem> = raw
        .history
        .iter()
        .map(|item| (item.sequence, item))
        .collect();
    let mut groups = Vec::new();
    if let Some(summary) = &raw.summary {
        groups.push(CompactionHistoryGroup {
            messages: vec![Message {
                role: Role::System,
                contents: vec![Content::Text(summary.content.clone())],
                ..Message::default()
            }],
            end_watermark: summary.watermark,
            is_complete: true,
        });
    }
    for group in assemble_history_groups(&members) {
        groups.push(CompactionHistoryGroup {
            end_watermark: *group.member_sequences.last().expect("group has its owner"),
            messages: group
                .member_sequences
                .iter()
                .map(|sequence| items[sequence].message.clone())
                .collect(),
            is_complete: group.is_complete,
        });
    }
    groups
}

fn history_group_member(item: &ConversationHistoryItem) -> HistoryGroupMember {
    HistoryGroupMember {
        sequence: item.sequence,
        tool_owner_sequence: item.tool_owner_sequence,
        is_compaction_status: item.is_compaction_status,
        is_assistant: item.message.role == Role::Assistant,
        tool_call_ids: item
            .message
            .tool_calls
            .iter()
            .map(|call| call.id.clone())
            .collect(),
        tool_result_call_id: (item.message.role == Role::Tool)
            .then(|| item.message.tool_call_id.clone()),
    }
}
