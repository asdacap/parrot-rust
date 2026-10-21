//! Source tool-terminal identity and result projection, inside one caller-owned transaction.
//! The owner must roll back on errors; these helpers never commit or publish.
use super::error::EventError;
use super::projection::{insert_terminal_parts, project, read_terminal_parts};
use super::types::{
    ConversationItem, ConversationMessage, ConversationOrigin, ConversationPart, MessageRole,
    ProviderContent, ToolExecutionStatus, ToolExecutionTerminal,
};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

pub fn append_tool_terminal(
    tx: &Transaction<'_>,
    event: &Event,
    terminal: &ToolExecutionTerminal,
    clock: &dyn Fn() -> String,
) -> Result<bool, EventError> {
    if let Some(existing) = read_tool_terminal(tx, &event.agent_session_id, &terminal.tool_call_id)?
    {
        check_terminal(&existing, terminal)?;
        return Ok(false);
    }
    super::record(tx, event, None, &clock())?;
    insert_tool_terminal(tx, &event.agent_session_id, terminal, &clock())?;
    Ok(true)
}

pub fn append_tool_settlement(
    tx: &Transaction<'_>,
    event: &Event,
    assistant_sequence: i64,
    terminal: &ToolExecutionTerminal,
    clock: &dyn Fn() -> String,
) -> Result<Option<ConversationItem>, EventError> {
    match read_tool_terminal(tx, &event.agent_session_id, &terminal.tool_call_id)? {
        Some(existing) => check_terminal(&existing, terminal)?,
        None => {
            super::record(tx, event, None, &clock())?;
            insert_tool_terminal(tx, &event.agent_session_id, terminal, &clock())?;
        }
    }
    if has_tool_result(
        tx,
        &event.agent_session_id,
        assistant_sequence,
        &terminal.tool_call_id,
    )? {
        return Ok(None);
    }
    insert_tool_result(
        tx,
        &event.agent_session_id,
        assistant_sequence,
        terminal,
        &clock(),
    )
    .map(Some)
}

pub fn tool_terminals(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Vec<ToolExecutionTerminal>, EventError> {
    let mut statement = tx.prepare(
        "SELECT sequence, tool_call_id, tool_name, status, message FROM tool_execution_terminal \
         WHERE agent_session = ?1 ORDER BY sequence;",
    )?;
    let rows = statement.query_map([agent], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (sequence, tool_call_id, tool_name, status, message) = row?;
        Ok(ToolExecutionTerminal {
            tool_call_id,
            tool_name,
            status: parse_status(&status)?,
            result_parts: read_terminal_parts(tx, sequence)?,
            message,
        })
    })
    .collect()
}

pub fn tool_terminals_of_calls(
    tx: &Transaction<'_>,
    agent: &str,
    calls: &[String],
) -> Result<Vec<ToolExecutionTerminal>, EventError> {
    let mut terminals = Vec::with_capacity(calls.len());
    for call in calls {
        if let Some(terminal) = read_tool_terminal(tx, agent, call)? {
            terminals.push(terminal);
        }
    }
    Ok(terminals)
}

pub fn has_tool_synthetic(
    tx: &Transaction<'_>,
    agent: &str,
    assistant_sequence: i64,
) -> Result<bool, EventError> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM tool_batch_synthetic \
         WHERE agent_session = ?1 AND assistant_sequence = ?2);",
        params![agent, assistant_sequence],
        |row| row.get(0),
    )?)
}

pub fn append_tool_synthetic(
    tx: &Transaction<'_>,
    event: &Event,
    assistant_sequence: i64,
    image_parts: &[ConversationPart],
    clock: &dyn Fn() -> String,
) -> Result<Option<ConversationItem>, EventError> {
    if image_parts.is_empty()
        || image_parts
            .iter()
            .any(|part| !matches!(part, ConversationPart::ImageArtifact { .. }))
    {
        return Err(EventError::Argument(
            "A tool synthetic message requires image artifact parts.".into(),
        ));
    }
    if has_tool_synthetic(tx, &event.agent_session_id, assistant_sequence)? {
        return Ok(None);
    }
    super::record(tx, event, None, &clock())?;
    let item = project(
        tx,
        &event.agent_session_id,
        &ConversationMessage {
            origin: ConversationOrigin::Tool,
            role: MessageRole::User,
            parts: image_parts.to_vec(),
            tool_calls: Vec::new(),
            tool_call_id: String::new(),
            provider_content: ProviderContent::default(),
        },
        &clock(),
    )?;
    tx.execute(
        "INSERT INTO tool_batch_synthetic (agent_session, assistant_sequence, item_sequence) \
         VALUES (?1, ?2, ?3);",
        params![event.agent_session_id, assistant_sequence, item.sequence],
    )?;
    Ok(Some(item))
}

fn has_tool_result(
    tx: &Transaction<'_>,
    agent: &str,
    assistant_sequence: i64,
    call: &str,
) -> Result<bool, EventError> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM tool_batch_result \
         WHERE agent_session = ?1 AND assistant_sequence = ?2 AND tool_call_id = ?3);",
        params![agent, assistant_sequence, call],
        |row| row.get(0),
    )?)
}

fn read_tool_terminal(
    tx: &Transaction<'_>,
    agent: &str,
    call: &str,
) -> Result<Option<ToolExecutionTerminal>, EventError> {
    let row = tx
        .query_row(
            "SELECT sequence, tool_name, status, message FROM tool_execution_terminal \
         WHERE agent_session = ?1 AND tool_call_id = ?2;",
            params![agent, call],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(sequence, tool_name, status, message)| {
        Ok(ToolExecutionTerminal {
            tool_call_id: call.to_owned(),
            tool_name,
            status: parse_status(&status)?,
            result_parts: read_terminal_parts(tx, sequence)?,
            message,
        })
    })
    .transpose()
}

fn check_terminal(
    existing: &ToolExecutionTerminal,
    proposed: &ToolExecutionTerminal,
) -> Result<(), EventError> {
    if existing != proposed {
        return Err(EventError::Conflict(format!(
            "tool call {} was already settled differently",
            proposed.tool_call_id
        )));
    }
    Ok(())
}

fn parse_status(value: &str) -> Result<ToolExecutionStatus, EventError> {
    match value {
        "finished" => Ok(ToolExecutionStatus::Finished),
        "cancelled" => Ok(ToolExecutionStatus::Cancelled),
        "error" => Ok(ToolExecutionStatus::Error),
        "image_budget_exceeded" => Ok(ToolExecutionStatus::ImageBudgetExceeded),
        _ => Err(EventError::Invalid(format!(
            "unknown tool execution status {value}"
        ))),
    }
}

fn insert_tool_terminal(
    tx: &Transaction<'_>,
    agent: &str,
    terminal: &ToolExecutionTerminal,
    at: &str,
) -> Result<(), EventError> {
    tx.execute(
        "INSERT INTO tool_execution_terminal (agent_session, tool_call_id, tool_name, status, message, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
        params![agent, terminal.tool_call_id, terminal.tool_name, terminal.status.as_str(), terminal.message, at],
    )?;
    insert_terminal_parts(tx, tx.last_insert_rowid(), &terminal.result_parts)
}

fn insert_tool_result(
    tx: &Transaction<'_>,
    agent: &str,
    assistant_sequence: i64,
    terminal: &ToolExecutionTerminal,
    at: &str,
) -> Result<ConversationItem, EventError> {
    let mut parts: Vec<_> = terminal
        .result_parts
        .iter()
        .filter(|part| matches!(part, ConversationPart::Text(_)))
        .cloned()
        .collect();
    if parts.is_empty() {
        parts.push(ConversationPart::text(&terminal.message));
    }
    let item = project(
        tx,
        agent,
        &ConversationMessage {
            origin: ConversationOrigin::Tool,
            role: MessageRole::Tool,
            parts,
            tool_calls: Vec::new(),
            tool_call_id: terminal.tool_call_id.clone(),
            provider_content: ProviderContent::default(),
        },
        at,
    )?;
    tx.execute(
        "INSERT INTO tool_batch_result (agent_session, assistant_sequence, tool_call_id, item_sequence) \
         VALUES (?1, ?2, ?3, ?4);",
        params![agent, assistant_sequence, terminal.tool_call_id, item.sequence],
    )?;
    Ok(item)
}
