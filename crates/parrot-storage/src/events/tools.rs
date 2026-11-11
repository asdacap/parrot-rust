//! Conversation-owned tool results, inside one caller-owned transaction.
//! The owner must roll back on errors; these helpers never commit or publish.
use super::error::EventError;
use super::projection::{project, read_parts};
use super::types::{
    ConversationItem, ConversationMessage, ConversationOrigin, ConversationPart, MessageRole,
    ProviderContent, ToolExecutionStatus, ToolExecutionTerminal,
};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

pub fn append_tool_settlement(
    tx: &Transaction<'_>,
    event: &Event,
    assistant_sequence: i64,
    terminal: &ToolExecutionTerminal,
    clock: &dyn Fn() -> String,
) -> Result<Option<ConversationItem>, EventError> {
    if let Some((existing, owner)) =
        read_tool_terminal(tx, &event.agent_session_id, &terminal.tool_call_id)?
    {
        if existing != *terminal || owner != Some(assistant_sequence) {
            return Err(EventError::Conflict(format!(
                "tool call {} was already settled differently",
                terminal.tool_call_id
            )));
        }
        return Ok(None);
    }
    super::record(tx, event, &clock())?;
    let item = project(
        tx,
        &event.agent_session_id,
        &ConversationMessage {
            origin: ConversationOrigin::Tool,
            role: MessageRole::Tool,
            parts: terminal.result_parts.clone(),
            tool_calls: Vec::new(),
            tool_call_id: terminal.tool_call_id.clone(),
            provider_content: ProviderContent::default(),
        },
        &clock(),
    )?;
    tx.execute(
        "UPDATE conversation_item SET tool_owner_sequence = ?1, tool_result_name = ?2, tool_result_status = ?3 \
         WHERE sequence = ?4;",
        params![
            assistant_sequence,
            terminal.tool_name,
            terminal.status.as_str(),
            item.sequence
        ],
    )?;
    Ok(Some(item))
}

pub fn tool_terminals_of_calls(
    tx: &Transaction<'_>,
    agent: &str,
    calls: &[String],
) -> Result<Vec<ToolExecutionTerminal>, EventError> {
    let mut terminals = Vec::with_capacity(calls.len());
    for call in calls {
        if let Some((terminal, _)) = read_tool_terminal(tx, agent, call)? {
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
        "SELECT EXISTS (SELECT 1 FROM conversation_item \
         WHERE agent_session = ?1 AND tool_owner_sequence = ?2 AND origin = 'tool' AND role = 'user');",
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
    super::record(tx, event, &clock())?;
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
        "UPDATE conversation_item SET tool_owner_sequence = ?2 \
         WHERE agent_session = ?1 AND sequence = ?3;",
        params![event.agent_session_id, assistant_sequence, item.sequence],
    )?;
    Ok(Some(item))
}

fn read_tool_terminal(
    tx: &Transaction<'_>,
    agent: &str,
    call: &str,
) -> Result<Option<(ToolExecutionTerminal, Option<i64>)>, EventError> {
    let row = tx
        .query_row(
            "SELECT sequence, tool_result_name, tool_result_status, tool_owner_sequence FROM conversation_item \
             WHERE agent_session = ?1 AND tool_call_id = ?2 AND tool_result_status IS NOT NULL;",
            params![agent, call],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(sequence, tool_name, status, owner)| {
        Ok((
            ToolExecutionTerminal {
                tool_call_id: call.to_owned(),
                tool_name,
                status: parse_status(&status)?,
                result_parts: read_parts(tx, "conversation_part", "item_sequence", &sequence)?,
            },
            owner,
        ))
    })
    .transpose()
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
