use super::model::CompactionHistoryGroup;
use parrot_core::{
    ConversationHistoryItem, ConversationState, HistoryGroupMember, Role, assemble_history_groups,
};
use std::collections::BTreeMap;

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
            messages: vec![summary.message.clone()],
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
