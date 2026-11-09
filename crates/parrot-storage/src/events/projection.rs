//! Source repository projection writes. The caller owns the whole transaction.
use super::{error::EventError, types::*};
use crate::artifacts::ImageArtifactMetadata;
use rusqlite::{OptionalExtension, Row, ToSql, Transaction, params};

pub fn read_latest_final_phase(tx: &Transaction<'_>, agent: &str) -> Result<bool, EventError> {
    Ok(tx
        .query_row(
            "SELECT final_phase FROM conversation_item WHERE agent_session = ?1 AND kind <> 'compaction' ORDER BY sequence DESC LIMIT 1;",
            [agent],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

pub fn insert_conversation_item(
    tx: &Transaction<'_>,
    agent: &str,
    message: &ConversationMessage,
    at: &str,
) -> Result<(i64, bool), EventError> {
    let final_phase = read_latest_final_phase(tx, agent)?;
    let provider_id = if message.provider_content.content.is_empty() {
        ""
    } else {
        message.provider_content.provider_id.as_str()
    };
    tx.execute(
        "INSERT INTO conversation_item (agent_session, origin, role, tool_call_id, created_at, final_phase, provider_id, provider_content) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8);",
        params![
            agent,
            message.origin.as_str(),
            message.role.as_str(),
            message.tool_call_id,
            at,
            final_phase,
            provider_id,
            message.provider_content.content
        ],
    )?;
    Ok((tx.last_insert_rowid(), final_phase))
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
            "INSERT INTO input_part (input_id, position, kind, text, artifact_id, media_type, display_name, byte_length, width, height, frame_count, aggregate_pixels, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13);"
        }
        ("conversation_part", "item_sequence") => {
            "INSERT INTO conversation_part (item_sequence, position, kind, text, artifact_id, media_type, display_name, byte_length, width, height, frame_count, aggregate_pixels, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13);"
        }
        _ => return Err(EventError::Invalid("unknown repository part table".into())),
    };
    for (position, part) in parts.iter().enumerate() {
        let (kind, text, artifact, media, display, length, width, height, frames, pixels, origin) =
            match part {
                ConversationPart::Text(text) => {
                    ("text", text.as_str(), "", "", "", 0, 0, 0, 0, 0, "")
                }
                ConversationPart::ImageArtifact { metadata } => (
                    "image_artifact",
                    "",
                    metadata.artifact_id.as_str(),
                    metadata.media_type.as_str(),
                    metadata.display_name.as_str(),
                    metadata.byte_length,
                    metadata.width,
                    metadata.height,
                    metadata.frame_count,
                    metadata.aggregate_pixels,
                    metadata.origin.as_str(),
                ),
            };
        tx.execute(
            sql,
            params![
                owner,
                position as i64,
                kind,
                text,
                artifact,
                media,
                display,
                length,
                width,
                height,
                frames,
                pixels,
                origin
            ],
        )?;
    }
    Ok(())
}

pub fn read_part(row: &Row<'_>) -> Result<ConversationPart, EventError> {
    let kind: String = row.get("kind")?;
    match kind.as_str() {
        "text" => Ok(ConversationPart::Text(row.get("text")?)),
        "image_artifact" => {
            let artifact_id: String = row.get("artifact_id")?;
            Ok(ConversationPart::image(ImageArtifactMetadata {
                sha256: artifact_id.clone(),
                artifact_id,
                media_type: row.get("media_type")?,
                display_name: row.get("display_name")?,
                byte_length: row.get("byte_length")?,
                width: row.get("width")?,
                height: row.get("height")?,
                frame_count: row.get("frame_count")?,
                aggregate_pixels: row.get("aggregate_pixels")?,
                origin: row.get("origin")?,
            }))
        }
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
            "SELECT kind, text, artifact_id, media_type, display_name, byte_length, width, height, frame_count, aggregate_pixels, origin FROM input_part WHERE input_id = ?1 ORDER BY position;"
        }
        ("conversation_part", "item_sequence") => {
            "SELECT kind, text, artifact_id, media_type, display_name, byte_length, width, height, frame_count, aggregate_pixels, origin FROM conversation_part WHERE item_sequence = ?1 AND kind <> 'tool_call' ORDER BY position;"
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

pub fn project(
    tx: &Transaction<'_>,
    agent: &str,
    message: &ConversationMessage,
    at: &str,
) -> Result<ConversationItem, EventError> {
    let (sequence, final_phase) = insert_conversation_item(tx, agent, message, at)?;
    insert_parts(
        tx,
        "conversation_part",
        "item_sequence",
        sequence,
        &message.parts,
    )?;
    for (position, call) in message.tool_calls.iter().enumerate() {
        tx.execute(
            "INSERT INTO conversation_part (item_sequence, position, kind, text, artifact_id, media_type, display_name, tool_call_id, tool_name, arguments) \
             VALUES (?1, ?2, 'tool_call', '', '', '', '', ?3, ?4, ?5);",
            params![
                sequence,
                (message.parts.len() + position) as i64,
                call.id,
                call.name,
                call.arguments_json
            ],
        )?;
    }
    Ok(ConversationItem {
        sequence,
        message: message.clone(),
        final_phase,
    })
}
