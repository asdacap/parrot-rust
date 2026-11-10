use std::collections::BTreeMap;

/// The facts that decide causal grouping for one durable history row. Compaction and
/// storage forks both reduce their row types to this view before grouping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGroupMember {
    pub sequence: i64,
    pub tool_owner_sequence: Option<i64>,
    pub is_compaction_status: bool,
    pub is_assistant: bool,
    pub tool_call_ids: Vec<String>,
    pub tool_result_call_id: Option<String>,
}

impl HistoryGroupMember {
    /// A causal group starts here only when an assistant issues tool calls.
    pub fn makes_tool_calls(&self) -> bool {
        self.is_assistant && !self.tool_call_ids.is_empty()
    }
}

/// One causal group: an owner row plus the tool results answering its calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGroup {
    pub owner_sequence: i64,
    pub member_sequences: Vec<i64>,
    pub is_complete: bool,
}

/// Groups rows that own history rows with their tool-result children, sorted by
/// sequence, and checks every tool call has a tool-role result inside its group.
/// Children whose owner row is absent are dropped. Compaction-status rows never group.
pub fn assemble_history_groups(members: &[HistoryGroupMember]) -> Vec<HistoryGroup> {
    let mut children: BTreeMap<i64, Vec<&HistoryGroupMember>> = BTreeMap::new();
    for member in members {
        if member.is_compaction_status {
            continue;
        }
        if let Some(owner) = member.tool_owner_sequence {
            children.entry(owner).or_default().push(member);
        }
    }
    let mut groups = Vec::new();
    for member in members {
        if member.is_compaction_status || member.tool_owner_sequence.is_some() {
            continue;
        }
        let mut group_members = vec![member];
        group_members.extend(children.remove(&member.sequence).into_iter().flatten());
        group_members.sort_by_key(|member| member.sequence);
        let owns_calls = member.makes_tool_calls();
        let is_complete = !owns_calls
            || member.tool_call_ids.iter().all(|call_id| {
                group_members.iter().any(|group_member| {
                    group_member.tool_result_call_id.as_deref() == Some(call_id.as_str())
                })
            });
        groups.push(HistoryGroup {
            owner_sequence: member.sequence,
            member_sequences: group_members
                .iter()
                .map(|group_member| group_member.sequence)
                .collect(),
            is_complete,
        });
    }
    groups
}
