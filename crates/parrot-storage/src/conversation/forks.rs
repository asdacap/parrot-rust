//! Checkpoints and fork copies execute inside one selected-session transaction.
use super::{ConversationRepository, EventError, compaction, reads, types::*};
use crate::events::{projection, timestamp};
use parrot_core::{HistoryGroupMember, assemble_history_groups};
use rusqlite::{Transaction, params};
use std::collections::{BTreeMap, BTreeSet};

/// Source fork argument attribution without changing the accepted T17 error API.
#[derive(Debug)]
pub enum ForkError {
    Argument {
        message: String,
        parameter_name: &'static str,
    },
    Repository(EventError),
}
impl ForkError {
    pub fn parameter_name(&self) -> Option<&str> {
        match self {
            Self::Argument { parameter_name, .. } => Some(parameter_name),
            Self::Repository(_) => None,
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Argument { message, .. } => message.clone(),
            Self::Repository(error) => error.to_string(),
        }
    }
    fn argument(message: impl Into<String>, parameter_name: &'static str) -> Self {
        Self::Argument {
            message: message.into(),
            parameter_name,
        }
    }
}
impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argument {
                message,
                parameter_name,
            } => write!(f, "{message} (Parameter '{parameter_name}')"),
            Self::Repository(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ForkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::Argument { .. } => None,
        }
    }
}
impl From<EventError> for ForkError {
    fn from(error: EventError) -> Self {
        Self::Repository(error)
    }
}
impl From<crate::database::DatabaseError> for ForkError {
    fn from(error: crate::database::DatabaseError) -> Self {
        EventError::from(error).into()
    }
}
impl From<rusqlite::Error> for ForkError {
    fn from(error: rusqlite::Error) -> Self {
        EventError::from(error).into()
    }
}
impl ConversationRepository {
    pub async fn latest_usable_checkpoint(
        &self,
        agent: String,
        title: String,
        before: i64,
    ) -> Result<Option<HistoryCheckpoint>, EventError> {
        self.events
            .transaction(move |tx| read_latest_usable_checkpoint(tx, &agent, &title, before))
            .await
    }

    pub async fn active_checkpoint_assistant_sequences(
        &self,
        agent: String,
    ) -> Result<BTreeSet<i64>, EventError> {
        self.events
            .transaction(move |tx| {
                let watermark =
                    compaction::read_snapshot(tx, &agent)?.map_or(0, |snapshot| snapshot.watermark);
                Ok(read_checkpoints(tx, &agent)?
                    .into_iter()
                    .filter(|checkpoint| checkpoint.assistant_sequence > watermark)
                    .map(|checkpoint| checkpoint.assistant_sequence)
                    .collect())
            })
            .await
    }

    pub async fn effective_conversation_groups(
        &self,
        agent: String,
    ) -> Result<EffectiveConversationHistory, EventError> {
        self.events
            .transaction(move |tx| read_effective_conversation_groups(tx, &agent))
            .await
    }

    pub async fn initialize_forked_agent_history(
        &self,
        source: String,
        destination: String,
        boundary: HistoryForkBoundary,
        selection: HistoryForkSelection,
        fallback: Option<HistoryForkSource>,
    ) -> Result<(), ForkError> {
        if source.trim().is_empty() {
            return Err(ForkError::argument(
                "The value cannot be an empty string or composed entirely of whitespace.",
                "sourceAgentSessionId",
            ));
        }
        if destination.trim().is_empty() {
            return Err(ForkError::argument(
                "The value cannot be an empty string or composed entirely of whitespace.",
                "destinationAgentSessionId",
            ));
        }
        if selection.kind == HistoryForkKind::Empty {
            return Ok(());
        }
        self.validate(&destination)?;
        let target = destination.clone();
        self.events
            .transact(move |tx| {
                initialize_fork(
                    tx, source, &target, boundary, &selection, fallback, &timestamp,
                )
            })
            .await?;
        self.rebuild_agent_history(&destination)
            .await
            .map_err(ForkError::from)
    }

    pub async fn cleanup_forked_agent_history(&self, agent: String) -> Result<(), EventError> {
        self.validate(&agent)?;
        let target = agent.clone();
        self.events
            .transaction(move |tx| {
                tx.execute(
                    "DELETE FROM conversation_item WHERE agent_session = ?1;",
                    [&target],
                )?;
                Ok(())
            })
            .await?;
        self.rebuild_agent_history(&agent).await
    }
}

pub(super) fn read_checkpoints(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Vec<HistoryCheckpoint>, EventError> {
    let mut read = tx.prepare("SELECT call.arguments, item.sequence, call.tool_call_id FROM conversation_part AS call JOIN conversation_item AS item ON item.sequence = call.item_sequence JOIN conversation_item AS result ON result.agent_session = item.agent_session AND result.tool_owner_sequence = item.sequence AND result.tool_call_id = call.tool_call_id AND result.role = 'tool' AND result.tool_result_name = 'set_checkpoint' AND result.tool_result_status = 'finished' WHERE item.agent_session = ?1 AND call.kind = 'tool_call' AND call.tool_name = 'set_checkpoint' ORDER BY item.sequence, call.position;")?;
    let mut rows = read.query([agent])?;
    let mut checkpoints = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let arguments: String = row.get(0)?;
        let Ok(mut fields) = serde_json::from_str::<BTreeMap<String, Option<String>>>(&arguments)
        else {
            continue;
        };
        if fields.len() != 1 {
            continue;
        }
        let Some(Some(title)) = fields.remove("title") else {
            continue;
        };
        checkpoints.insert(
            title.clone(),
            HistoryCheckpoint {
                title,
                assistant_sequence: row.get(1)?,
                tool_call_id: row.get(2)?,
            },
        );
    }
    Ok(checkpoints.into_values().collect())
}

pub fn read_latest_usable_checkpoint(
    tx: &Transaction<'_>,
    agent: &str,
    title: &str,
    before: i64,
) -> Result<Option<HistoryCheckpoint>, EventError> {
    Ok(read_checkpoints(tx, agent)?
        .into_iter()
        .find(|checkpoint| checkpoint.title == title)
        .filter(|checkpoint| checkpoint.assistant_sequence < before))
}

pub fn read_effective_conversation_groups(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<EffectiveConversationHistory, EventError> {
    let snapshot = compaction::read_snapshot(tx, agent)?;
    let status = if let Some(snapshot) = &snapshot {
        let sequence: Option<i64> = tx.query_row("SELECT compaction_status_sequence FROM conversation_item WHERE agent_session = ?1 AND kind = 'compaction' AND compaction_watermark = ?2;", params![agent,snapshot.watermark], |row| row.get(0))?;
        sequence
            .map(|sequence| reads::read_conversation_item(tx, sequence))
            .transpose()?
    } else {
        None
    };
    let items =
        reads::read_compaction_tail(tx, agent, snapshot.as_ref().map_or(0, |s| s.watermark))?;
    let mut owners = BTreeMap::new();
    let mut read = tx.prepare("SELECT tool_owner_sequence, sequence FROM conversation_item WHERE agent_session = ?1 AND tool_owner_sequence IS NOT NULL;")?;
    for row in read.query_map([agent], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (owner, child) = row?;
        owners.insert(child, owner);
    }
    let by_sequence: BTreeMap<_, _> = items.iter().map(|item| (item.sequence, item)).collect();
    let members: Vec<HistoryGroupMember> = items
        .iter()
        .map(|item| store_group_member(item, owners.get(&item.sequence).copied()))
        .collect();
    let member_by_sequence: BTreeMap<i64, HistoryGroupMember> = members
        .iter()
        .map(|member| (member.sequence, member.clone()))
        .collect();
    let mut groups = Vec::new();
    for group in assemble_history_groups(&members) {
        let owner = &member_by_sequence[&group.owner_sequence];
        let assistant_sequence = if owner.makes_tool_calls() {
            group.owner_sequence
        } else {
            0
        };
        groups.push(ConversationGroup {
            start_watermark: group.member_sequences[0] - 1,
            end_watermark: *group.member_sequences.last().expect("group has its owner"),
            items: group
                .member_sequences
                .iter()
                .map(|sequence| by_sequence[sequence].clone())
                .collect(),
            assistant_sequence,
            is_complete: group.is_complete,
        });
    }
    Ok(EffectiveConversationHistory {
        snapshot,
        status,
        groups,
    })
}

fn store_group_member(
    item: &ConversationItem,
    tool_owner_sequence: Option<i64>,
) -> HistoryGroupMember {
    HistoryGroupMember {
        sequence: item.sequence,
        tool_owner_sequence,
        is_compaction_status: false,
        is_assistant: item.message.role == MessageRole::Assistant,
        tool_call_ids: item
            .message
            .tool_calls
            .iter()
            .map(|call| call.id.clone())
            .collect(),
        tool_result_call_id: (item.message.role == MessageRole::Tool)
            .then(|| item.message.tool_call_id.clone()),
    }
}

pub fn select_safe_history_prefix(groups: &[ConversationGroup]) -> &[ConversationGroup] {
    let mut length = groups.iter().take_while(|group| group.is_complete).count();
    if length == groups.len() {
        return groups;
    }
    let mut cutoff = groups[length].start_watermark;
    for index in (0..length).rev() {
        if groups[index].end_watermark > cutoff {
            length = index;
            cutoff = groups[index].start_watermark;
        }
    }
    &groups[..length]
}

fn initialize_fork(
    tx: &Transaction<'_>,
    mut source: String,
    destination: &str,
    mut boundary: HistoryForkBoundary,
    selection: &HistoryForkSelection,
    fallback: Option<HistoryForkSource>,
    clock: &dyn Fn() -> String,
) -> Result<(), ForkError> {
    let mut effective = read_effective_conversation_groups(tx, &source)?;
    if let Some(fallback) =
        fallback.filter(|_| effective.snapshot.is_none() && effective.groups.is_empty())
    {
        if fallback.agent_session_id.trim().is_empty() {
            return Err(ForkError::argument(
                "The value cannot be an empty string or composed entirely of whitespace.",
                "fallback.AgentSessionId",
            ));
        }
        source = fallback.agent_session_id;
        boundary = fallback.boundary;
        effective = read_effective_conversation_groups(tx, &source)?;
    }
    let (preceding, before) = match boundary {
        HistoryForkBoundary::BeforeToolBatch {
            assistant_sequence,
            tool_call_id,
        } => {
            let index = effective.groups.iter().position(|group| group.assistant_sequence == assistant_sequence && group.items[0].message.tool_calls.iter().any(|call| call.id == tool_call_id)).ok_or_else(|| ForkError::argument("captured fork tool batch is unavailable in effective history; it may have been compacted", "boundary"))?;
            (&effective.groups[..index], assistant_sequence)
        }
        HistoryForkBoundary::AfterCompletedHistory => (effective.groups.as_slice(), i64::MAX),
        HistoryForkBoundary::AfterSafeHistoryPrefix => {
            let preceding = select_safe_history_prefix(&effective.groups);
            let before = if preceding.len() == effective.groups.len() {
                i64::MAX
            } else {
                effective.groups[preceding.len()].start_watermark + 1
            };
            (preceding, before)
        }
    };
    if preceding.iter().any(|group| !group.is_complete) {
        return Err(ForkError::argument(
            "parent history contains an incomplete tool batch",
            "boundary",
        ));
    }
    let unavailable = || {
        ForkError::argument(
            format!("checkpoint '{}' is unavailable", selection.title),
            "selection",
        )
    };
    let selected = if selection.kind == HistoryForkKind::Named {
        let checkpoint = read_latest_usable_checkpoint(tx, &source, &selection.title, before)?
            .ok_or_else(unavailable)?;
        if effective
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| checkpoint.assistant_sequence <= snapshot.watermark)
        {
            return Err(unavailable());
        }
        let index = preceding
            .iter()
            .position(|group| group.assistant_sequence == checkpoint.assistant_sequence)
            .filter(|index| preceding[*index].is_complete)
            .ok_or_else(unavailable)?;
        &preceding[index..]
    } else {
        preceding
    };
    let mut sequences = BTreeMap::new();
    if let Some(snapshot) = effective
        .snapshot
        .as_ref()
        .filter(|_| selection.kind == HistoryForkKind::Full)
    {
        let at = clock();
        let summary_sequence = compaction::save_snapshot(
            tx,
            destination,
            &CompactionSnapshot {
                summary: snapshot.summary.clone(),
                watermark: 0,
            },
            &at,
        )?;
        if let Some(status) = &effective.status {
            let sequence =
                projection::project(tx, destination, &status.message, &clock())?.sequence;
            tx.execute(
                "UPDATE conversation_item SET final_phase = ?1 WHERE sequence = ?2;",
                params![status.final_phase, sequence],
            )?;
            tx.execute(
                "UPDATE conversation_item SET compaction_status_sequence = ?1 WHERE sequence = ?2;",
                params![sequence, summary_sequence],
            )?;
        }
    }
    for group in selected {
        for item in &group.items {
            let sequence = projection::project(tx, destination, &item.message, &clock())?.sequence;
            tx.execute(
                "UPDATE conversation_item SET final_phase = ?1, tool_result_name = (SELECT tool_result_name FROM conversation_item WHERE sequence = ?2), tool_result_status = (SELECT tool_result_status FROM conversation_item WHERE sequence = ?2), status_slots = (SELECT status_slots FROM conversation_item WHERE sequence = ?2) WHERE sequence = ?3;",
                params![item.final_phase, item.sequence, sequence],
            )?;
            sequences.insert(item.sequence, sequence);
        }
        if group.assistant_sequence == 0 {
            continue;
        }
        let assistant = &group.items[0];
        for call in &assistant.message.tool_calls {
            let mut results = group.items.iter().filter(|item| {
                item.message.role == MessageRole::Tool && item.message.tool_call_id == call.id
            });
            let result = results.next().ok_or_else(|| {
                EventError::Invalid("Sequence contains no matching element".into())
            })?;
            if results.next().is_some() {
                return Err(EventError::Invalid(
                    "Sequence contains more than one matching element".into(),
                )
                .into());
            }
            let has_result: bool = tx.query_row(
                "SELECT tool_result_status IS NOT NULL FROM conversation_item WHERE sequence = ?1;",
                [result.sequence],
                |row| row.get(0),
            )?;
            if !has_result {
                return Err(EventError::Invalid(
                    "a complete tool group has no result metadata".into(),
                )
                .into());
            }
            tx.execute(
                "UPDATE conversation_item SET tool_owner_sequence = ?1 WHERE sequence = ?2;",
                params![
                    sequences[&group.assistant_sequence],
                    sequences[&result.sequence]
                ],
            )?;
        }
        let mut synthetics = group.items.iter().filter(|item| {
            item.message.role == MessageRole::User
                && item.message.origin == ConversationOrigin::Tool
        });
        if let Some(synthetic) = synthetics.next() {
            if synthetics.next().is_some() {
                return Err(EventError::Invalid(
                    "Sequence contains more than one matching element".into(),
                )
                .into());
            }
            tx.execute(
                "UPDATE conversation_item SET tool_owner_sequence = ?1 WHERE sequence = ?2;",
                params![
                    sequences[&group.assistant_sequence],
                    sequences[&synthetic.sequence]
                ],
            )?;
        }
    }
    Ok(())
}
