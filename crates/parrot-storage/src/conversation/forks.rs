//! Checkpoints and fork copies execute inside one selected-session transaction.
use super::{ConversationRepository, EventError, reads, types::*};
use crate::events::{projection, tools};
use rusqlite::{OptionalExtension, Transaction, params};
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
        use crate::database::DatabaseError;
        match error {
            EventError::Database(DatabaseError::Sqlite(
                rusqlite::Error::ToSqlConversionFailure(error),
            )) => match error.downcast::<ForkError>() {
                Ok(error) => *error,
                Err(error) => Self::Repository(EventError::Database(DatabaseError::Sqlite(
                    rusqlite::Error::ToSqlConversionFailure(error),
                ))),
            },
            error => Self::Repository(error),
        }
    }
}
impl From<rusqlite::Error> for ForkError {
    fn from(error: rusqlite::Error) -> Self {
        EventError::from(error).into()
    }
}
fn rollback_fork(error: ForkError) -> EventError {
    match error {
        ForkError::Repository(error) => error,
        error => EventError::Database(crate::database::DatabaseError::Sqlite(
            rusqlite::Error::ToSqlConversionFailure(Box::new(error)),
        )),
    }
}

impl ConversationRepository {
    pub async fn record_checkpoint(
        &self,
        agent: String,
        title: String,
        assistant_sequence: i64,
        tool_call_id: String,
    ) -> Result<bool, EventError> {
        let clock = self.events.context().timestamp.clone();
        self.events.transaction(move |tx| {
            let changed = tx.execute("INSERT OR IGNORE INTO history_checkpoint (agent_session, title, assistant_sequence, tool_call_id, created_at) SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS (SELECT 1 FROM conversation_tool_call AS call JOIN conversation_item AS item ON item.sequence = call.item_sequence WHERE item.agent_session = ?1 AND item.sequence = ?3 AND call.id = ?4 AND call.name = 'set_checkpoint');", params![agent,title,assistant_sequence,tool_call_id,clock()])? != 0;
            if !changed {
                let existing: Option<(String,i64)> = tx.query_row("SELECT title, assistant_sequence FROM history_checkpoint WHERE agent_session = ?1 AND tool_call_id = ?2;", params![agent,tool_call_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
                if existing.as_ref() != Some(&(title, assistant_sequence)) {
                    return Err(EventError::Conflict(format!("checkpoint tool call {tool_call_id} was already recorded differently")));
                }
            }
            Ok(true)
        }).await
    }

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
        self.events.transaction(move |tx| {
            let mut read = tx.prepare("SELECT checkpoint.assistant_sequence FROM history_checkpoint AS checkpoint JOIN (SELECT title, MAX(sequence) AS sequence FROM history_checkpoint WHERE agent_session = ?1 GROUP BY title) AS latest ON latest.sequence = checkpoint.sequence LEFT JOIN compaction_snapshot AS snapshot ON snapshot.agent_session = checkpoint.agent_session WHERE checkpoint.agent_session = ?1 AND (snapshot.watermark IS NULL OR checkpoint.assistant_sequence > snapshot.watermark);")?;
            Ok(read.query_map([agent], |row| row.get(0))?.collect::<Result<BTreeSet<_>,_>>()?)
        }).await
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
        let clock = self.events.context().timestamp.clone();
        let target = destination.clone();
        self.events
            .transaction(move |tx| {
                initialize_fork(tx, source, &target, boundary, &selection, fallback, &*clock)
                    .map_err(rollback_fork)
            })
            .await
            .map_err(ForkError::from)?;
        self.rebuild_agent_history(&destination)
            .await
            .map_err(ForkError::from)
    }

    pub async fn cleanup_forked_agent_history(&self, agent: String) -> Result<(), EventError> {
        self.validate(&agent)?;
        let target = agent.clone();
        self.events
            .transaction(move |tx| {
                for table in [
                    "history_checkpoint",
                    "final_provider_request_prompt",
                    "compaction_status",
                    "compaction_snapshot",
                    "tool_execution_terminal",
                    "conversation_item",
                    "agent_history",
                    "message",
                ] {
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE agent_session = ?1;"),
                        [&target],
                    )?;
                }
                Ok(())
            })
            .await?;
        self.rebuild_agent_history(&agent).await
    }
}

pub fn read_latest_usable_checkpoint(
    tx: &Transaction<'_>,
    agent: &str,
    title: &str,
    before: i64,
) -> Result<Option<HistoryCheckpoint>, EventError> {
    let checkpoint = tx.query_row("SELECT title, assistant_sequence, tool_call_id FROM history_checkpoint WHERE agent_session = ?1 AND title = ?2 ORDER BY sequence DESC LIMIT 1;", params![agent,title], |row| Ok(HistoryCheckpoint { title: row.get(0)?, assistant_sequence: row.get(1)?, tool_call_id: row.get(2)? })).optional()?;
    Ok(checkpoint.filter(|checkpoint| checkpoint.assistant_sequence < before))
}

pub fn read_effective_conversation_groups(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<EffectiveConversationHistory, EventError> {
    projection::ensure_conversation_projection(tx)?;
    let snapshot = tx
        .query_row(
            "SELECT summary, watermark FROM compaction_snapshot WHERE agent_session = ?1;",
            [agent],
            |row| {
                Ok(CompactionSnapshot {
                    summary: row.get(0)?,
                    watermark: row.get(1)?,
                })
            },
        )
        .optional()?;
    let status = if let Some(snapshot) = &snapshot {
        let sequence: Option<i64> = tx.query_row("SELECT status_conversation_sequence FROM compaction_status WHERE agent_session = ?1 AND watermark = ?2;", params![agent,snapshot.watermark], |row| row.get(0)).optional()?;
        sequence
            .map(|sequence| reads::read_conversation_item(tx, sequence))
            .transpose()?
    } else {
        None
    };
    let items =
        reads::read_compaction_tail(tx, agent, snapshot.as_ref().map_or(0, |s| s.watermark))?;
    let mut owners = BTreeMap::new();
    let mut read = tx.prepare("SELECT assistant_sequence, item_sequence FROM tool_batch_result WHERE agent_session = ?1 UNION ALL SELECT assistant_sequence, item_sequence FROM tool_batch_synthetic WHERE agent_session = ?1;")?;
    for row in read.query_map([agent], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (owner, child) = row?;
        owners.insert(child, owner);
    }
    let by_sequence: BTreeMap<_, _> = items.iter().map(|item| (item.sequence, item)).collect();
    let mut owned_items: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (&child, &owner) in &owners {
        owned_items.entry(owner).or_default().push(child);
    }
    let mut groups = Vec::new();
    for item in &items {
        if owners.contains_key(&item.sequence) {
            continue;
        }
        let mut members = vec![item.clone()];
        members.extend(
            owned_items
                .get(&item.sequence)
                .into_iter()
                .flatten()
                .filter_map(|sequence| by_sequence.get(sequence).map(|item| (*item).clone())),
        );
        members.sort_by_key(|member| member.sequence);
        let assistant_sequence =
            if item.message.role == MessageRole::Assistant && !item.message.tool_calls.is_empty() {
                item.sequence
            } else {
                0
            };
        let is_complete = assistant_sequence == 0
            || item.message.tool_calls.iter().all(|call| {
                members.iter().any(|member| {
                    member.message.role == MessageRole::Tool
                        && member.message.tool_call_id == call.id
                })
            });
        groups.push(ConversationGroup {
            start_watermark: members[0].sequence - 1,
            end_watermark: members.last().expect("group has its owner").sequence,
            items: members,
            assistant_sequence,
            is_complete,
        });
    }
    Ok(EffectiveConversationHistory {
        snapshot,
        status,
        groups,
    })
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
    projection::ensure_conversation_projection(tx)?;
    projection::ensure_agent_history_projection(tx)?;
    let mut sequences = BTreeMap::new();
    if let Some(snapshot) = effective
        .snapshot
        .as_ref()
        .filter(|_| selection.kind == HistoryForkKind::Full)
    {
        let at = clock();
        tx.execute("INSERT INTO compaction_snapshot (agent_session, summary, watermark, created_at) VALUES (?1, ?2, 0, ?3);", params![destination,snapshot.summary,at])?;
        tx.execute("INSERT INTO agent_history (agent_session, kind, conversation_sequence, summary, watermark, created_at) VALUES (?1, 'compaction', NULL, ?2, 0, ?3);", params![destination,snapshot.summary,at])?;
        if let Some(status) = &effective.status {
            let sequence =
                projection::project(tx, destination, &status.message, &clock())?.sequence;
            tx.execute("INSERT INTO compaction_status (agent_session, watermark, status_conversation_sequence) VALUES (?1, 0, ?2);", params![destination,sequence])?;
        }
    }
    for group in selected {
        for item in &group.items {
            sequences.insert(
                item.sequence,
                projection::project(tx, destination, &item.message, &clock())?.sequence,
            );
        }
        if group.assistant_sequence == 0 {
            continue;
        }
        let assistant = &group.items[0];
        for call in &assistant.message.tool_calls {
            let terminal =
                tools::tool_terminals_of_calls(tx, &source, std::slice::from_ref(&call.id))?
                    .pop()
                    .ok_or_else(|| {
                        EventError::Invalid("a complete tool group has no terminal".into())
                    })?;
            clone_terminal(tx, destination, &terminal, &clock())?;
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
            tx.execute("INSERT INTO tool_batch_result (agent_session, assistant_sequence, tool_call_id, item_sequence) VALUES (?1, ?2, ?3, ?4);", params![destination,sequences[&group.assistant_sequence],call.id,sequences[&result.sequence]])?;
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
            tx.execute("INSERT INTO tool_batch_synthetic (agent_session, assistant_sequence, item_sequence) VALUES (?1, ?2, ?3);", params![destination,sequences[&group.assistant_sequence],sequences[&synthetic.sequence]])?;
        }
    }
    for group in selected.iter().filter(|group| group.assistant_sequence > 0) {
        let mut read = tx.prepare("SELECT title, tool_call_id FROM history_checkpoint WHERE agent_session = ?1 AND assistant_sequence = ?2 ORDER BY sequence;")?;
        let checkpoints = read
            .query_map(params![source, group.assistant_sequence], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (title, call) in checkpoints {
            tx.execute("INSERT INTO history_checkpoint (agent_session, title, assistant_sequence, tool_call_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5);", params![destination,title,sequences[&group.assistant_sequence],call,clock()])?;
        }
    }
    Ok(())
}

// P03 approved this fork-local row clone while accepted T17 remains frozen.
// Part encoding remains the shared T17 transaction helper; no event is appended.
fn clone_terminal(
    tx: &Transaction<'_>,
    agent: &str,
    terminal: &crate::events::ToolExecutionTerminal,
    at: &str,
) -> Result<(), EventError> {
    tx.execute("INSERT INTO tool_execution_terminal (agent_session, tool_call_id, tool_name, status, message, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6);", params![agent,terminal.tool_call_id,terminal.tool_name,terminal.status.as_str(),terminal.message,at])?;
    projection::insert_terminal_parts(tx, tx.last_insert_rowid(), &terminal.result_parts)
}
