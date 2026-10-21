//! Source repository projection writes. The caller owns the whole transaction.
use super::{error::EventError, types::*};
use rusqlite::{OptionalExtension, Row, ToSql, Transaction, params};

fn projection_current(tx: &Transaction<'_>, name: &str) -> Result<bool, EventError> {
    let version: Option<i64> = tx
        .query_row(
            "SELECT version FROM projection_version WHERE name = ?1;",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(version.is_some_and(|version| version >= 1))
}

fn mark_projection(tx: &Transaction<'_>, name: &str) -> Result<(), EventError> {
    tx.execute(
        "INSERT OR REPLACE INTO projection_version (name, version) VALUES (?1, 1);",
        [name],
    )?;
    Ok(())
}

pub fn ensure_conversation_projection(tx: &Transaction<'_>) -> Result<(), EventError> {
    if projection_current(tx, "conversation")? {
        return Ok(());
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM conversation_item;", [], |row| {
        row.get(0)
    })?;
    if count == 0 {
        tx.execute(
            "INSERT INTO conversation_item (sequence, agent_session, origin, role, tool_call_id, created_at) \
             SELECT sequence, agent_session, CASE role WHEN 'system' THEN 'system' \
             WHEN 'user' THEN 'user_input' WHEN 'tool' THEN 'tool' ELSE 'model' END, \
             role, '', created_at FROM message ORDER BY sequence;",
            [],
        )?;
        tx.execute(
            "INSERT INTO conversation_part \
             (item_sequence, position, kind, text, artifact_id, media_type, display_name) \
             SELECT sequence, 0, 'text', content, '', '', '' FROM message ORDER BY sequence;",
            [],
        )?;
    }
    mark_projection(tx, "conversation")
}

pub fn ensure_agent_history_projection(tx: &Transaction<'_>) -> Result<(), EventError> {
    if projection_current(tx, "agent-history")? {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO agent_history \
         (agent_session, kind, conversation_sequence, summary, watermark, created_at) \
         SELECT item.agent_session, 'message', item.sequence, '', 0, item.created_at \
         FROM conversation_item AS item \
         LEFT JOIN compaction_snapshot AS snapshot ON snapshot.agent_session = item.agent_session \
         WHERE snapshot.watermark IS NULL OR item.sequence <= snapshot.watermark \
         ORDER BY item.sequence;",
        [],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO agent_history \
         (agent_session, kind, conversation_sequence, summary, watermark, created_at) \
         SELECT agent_session, 'compaction', NULL, summary, watermark, created_at \
         FROM compaction_snapshot ORDER BY created_at, agent_session;",
        [],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO agent_history \
         (agent_session, kind, conversation_sequence, summary, watermark, created_at) \
         SELECT item.agent_session, 'message', item.sequence, '', 0, item.created_at \
         FROM conversation_item AS item \
         JOIN compaction_snapshot AS snapshot ON snapshot.agent_session = item.agent_session \
         WHERE item.sequence > snapshot.watermark ORDER BY item.sequence;",
        [],
    )?;
    mark_projection(tx, "agent-history")
}

pub fn insert_agent_history_message(
    tx: &Transaction<'_>,
    agent: &str,
    conversation_sequence: i64,
    at: &str,
) -> Result<(), EventError> {
    ensure_agent_history_projection(tx)?;
    tx.execute(
        "INSERT OR IGNORE INTO agent_history \
         (agent_session, kind, conversation_sequence, summary, watermark, created_at) \
         VALUES (?1, 'message', ?2, '', 0, ?3);",
        params![agent, conversation_sequence, at],
    )?;
    Ok(())
}

pub fn insert_conversation_item(
    tx: &Transaction<'_>,
    agent: &str,
    message: &ConversationMessage,
    at: &str,
) -> Result<i64, EventError> {
    tx.execute(
        "INSERT INTO conversation_item (agent_session, origin, role, tool_call_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5);",
        params![
            agent,
            message.origin.as_str(),
            message.role.as_str(),
            message.tool_call_id,
            at
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

pub fn insert_parts(
    tx: &Transaction<'_>,
    table: &str,
    owner_column: &str,
    owner: impl ToSql,
    parts: &[ConversationPart],
) -> Result<(), EventError> {
    let sql = match (table, owner_column) {
        ("input_part", "input_id") => {
            "INSERT INTO input_part (input_id, position, kind, text, artifact_id, media_type, display_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);"
        }
        ("conversation_part", "item_sequence") => {
            "INSERT INTO conversation_part (item_sequence, position, kind, text, artifact_id, media_type, display_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);"
        }
        ("tool_execution_result_part", "terminal_sequence") => {
            "INSERT INTO tool_execution_result_part (terminal_sequence, position, kind, text, artifact_id, media_type, display_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);"
        }
        _ => return Err(EventError::Invalid("unknown repository part table".into())),
    };
    for (position, part) in parts.iter().enumerate() {
        let (kind, text, artifact, media, display) = match part {
            ConversationPart::Text(text) => ("text", text.as_str(), "", "", ""),
            ConversationPart::ImageArtifact {
                artifact_id,
                media_type,
                display_name,
            } => (
                "image_artifact",
                "",
                artifact_id.as_str(),
                media_type.as_str(),
                display_name.as_str(),
            ),
        };
        tx.execute(
            sql,
            params![owner, position as i64, kind, text, artifact, media, display],
        )?;
    }
    Ok(())
}

pub fn read_part(row: &Row<'_>) -> Result<ConversationPart, EventError> {
    let kind: String = row.get("kind")?;
    match kind.as_str() {
        "text" => Ok(ConversationPart::Text(row.get("text")?)),
        "image_artifact" => Ok(ConversationPart::ImageArtifact {
            artifact_id: row.get("artifact_id")?,
            media_type: row.get("media_type")?,
            display_name: row.get("display_name")?,
        }),
        _ => Err(EventError::Invalid(format!(
            "unknown conversation part kind {kind}"
        ))),
    }
}

pub fn read_parts(
    tx: &Transaction<'_>,
    table: &str,
    owner_column: &str,
    owner: &dyn ToSql,
) -> Result<Vec<ConversationPart>, EventError> {
    let sql = match (table, owner_column) {
        ("input_part", "input_id") => {
            "SELECT kind, text, artifact_id, media_type, display_name FROM input_part WHERE input_id = ?1 ORDER BY position;"
        }
        ("conversation_part", "item_sequence") => {
            "SELECT kind, text, artifact_id, media_type, display_name FROM conversation_part WHERE item_sequence = ?1 ORDER BY position;"
        }
        ("tool_execution_result_part", "terminal_sequence") => {
            "SELECT kind, text, artifact_id, media_type, display_name FROM tool_execution_result_part WHERE terminal_sequence = ?1 ORDER BY position;"
        }
        _ => return Err(EventError::Invalid("unknown repository part table".into())),
    };
    let mut statement = tx.prepare(sql)?;
    let mut rows = statement.query([owner])?;
    let mut parts = Vec::new();
    while let Some(row) = rows.next()? {
        parts.push(read_part(row)?);
    }
    Ok(parts)
}

pub fn insert_input_parts(
    tx: &Transaction<'_>,
    input_id: &str,
    parts: &[ConversationPart],
) -> Result<(), EventError> {
    insert_parts(tx, "input_part", "input_id", input_id, parts)
}

pub fn insert_terminal_parts(
    tx: &Transaction<'_>,
    sequence: i64,
    parts: &[ConversationPart],
) -> Result<(), EventError> {
    insert_parts(
        tx,
        "tool_execution_result_part",
        "terminal_sequence",
        sequence,
        parts,
    )
}

pub fn read_terminal_parts(
    tx: &Transaction<'_>,
    sequence: i64,
) -> Result<Vec<ConversationPart>, EventError> {
    read_parts(
        tx,
        "tool_execution_result_part",
        "terminal_sequence",
        &sequence,
    )
}

pub fn read_input_parts(
    tx: &Transaction<'_>,
    input_id: &str,
    legacy_content: &str,
) -> Result<Vec<ConversationPart>, EventError> {
    let parts = read_parts(tx, "input_part", "input_id", &input_id)?;
    Ok(if parts.is_empty() {
        vec![ConversationPart::Text(legacy_content.into())]
    } else {
        parts
    })
}

pub fn insert_tool_calls(
    tx: &Transaction<'_>,
    sequence: i64,
    calls: &[ToolCall],
) -> Result<(), EventError> {
    for (position, call) in calls.iter().enumerate() {
        tx.execute(
            "INSERT INTO conversation_tool_call (item_sequence, position, id, name, arguments) \
             VALUES (?1, ?2, ?3, ?4, ?5);",
            params![
                sequence,
                position as i64,
                call.id,
                call.name,
                call.arguments_json
            ],
        )?;
    }
    Ok(())
}

pub fn insert_provider_content(
    tx: &Transaction<'_>,
    sequence: i64,
    content: &ProviderContent,
) -> Result<(), EventError> {
    if !content.content.is_empty() {
        tx.execute(
            "INSERT INTO conversation_provider_content (item_sequence, provider, content) \
             VALUES (?1, ?2, ?3);",
            params![sequence, content.provider_id, content.content],
        )?;
    }
    Ok(())
}

pub fn project(
    tx: &Transaction<'_>,
    agent: &str,
    message: &ConversationMessage,
    at: &str,
) -> Result<ConversationItem, EventError> {
    ensure_conversation_projection(tx)?;
    tx.execute(
        "INSERT INTO message (agent_session, role, content, created_at) VALUES (?1, ?2, ?3, ?4);",
        params![agent, message.role.as_str(), content(&message.parts), at],
    )?;
    let sequence = insert_conversation_item(tx, agent, message, at)?;
    insert_parts(
        tx,
        "conversation_part",
        "item_sequence",
        sequence,
        &message.parts,
    )?;
    insert_tool_calls(tx, sequence, &message.tool_calls)?;
    insert_provider_content(tx, sequence, &message.provider_content)?;
    insert_agent_history_message(tx, agent, sequence, at)?;
    Ok(ConversationItem {
        sequence,
        message: message.clone(),
    })
}
