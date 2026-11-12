//! Structured reads use the same selected-session transaction as projection writes.
use super::{ConversationRepository, EventError, types::*};
use crate::database::Transaction;
use crate::events::projection::read_parts;
use rusqlite::{OptionalExtension, params};

fn parse_origin(value: &str) -> Result<ConversationOrigin, EventError> {
    match value {
        "system" => Ok(ConversationOrigin::System),
        "user_input" => Ok(ConversationOrigin::UserInput),
        "model" => Ok(ConversationOrigin::Model),
        "tool" => Ok(ConversationOrigin::Tool),
        _ => Err(EventError::Invalid(format!(
            "unknown conversation origin {value}"
        ))),
    }
}
fn parse_role(value: &str) -> Result<MessageRole, EventError> {
    match value {
        "system" => Ok(MessageRole::System),
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        "tool" => Ok(MessageRole::Tool),
        _ => Err(EventError::Invalid(format!(
            "unknown conversation role {value}"
        ))),
    }
}
fn read_message(
    tx: &Transaction<'_>,
    sequence: i64,
    origin: &str,
    role: &str,
    tool_call_id: String,
    final_phase: bool,
    provider_content: ProviderContent,
) -> Result<ConversationItem, EventError> {
    let origin = parse_origin(origin)?;
    let role = parse_role(role)?;
    let parts = read_parts(tx, "conversation_part", "item_sequence", &sequence)?;
    let mut calls = tx.prepare("SELECT tool_call_id, tool_name, arguments FROM conversation_part WHERE item_sequence = ?1 AND kind = 'tool_call' ORDER BY position;")?;
    let tool_calls = calls
        .query_map([sequence], |row| {
            Ok(ToolCall {
                id: row.get(0)?,
                name: row.get(1)?,
                arguments_json: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConversationItem {
        sequence,
        final_phase,
        message: ConversationMessage {
            origin,
            role,
            parts,
            tool_calls,
            tool_call_id,
            provider_content,
        },
    })
}

pub fn read_conversation_item(
    tx: &Transaction<'_>,
    sequence: i64,
) -> Result<ConversationItem, EventError> {
    let item = tx
        .query_row(
            "SELECT origin, role, tool_call_id, final_phase, provider_id, provider_content FROM conversation_item WHERE sequence = ?1 AND kind <> 'compaction';",
            [sequence],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    ProviderContent {
                        provider_id: row.get(4)?,
                        content: row.get(5)?,
                    },
                ))
            },
        )
        .optional()?;
    let (origin, role, call, final_phase, provider_content) = item.ok_or_else(|| {
        EventError::Invalid(format!("Conversation item {sequence} does not exist."))
    })?;
    read_message(
        tx,
        sequence,
        &origin,
        &role,
        call,
        final_phase,
        provider_content,
    )
}
pub fn read_conversation(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
) -> Result<Vec<ConversationItem>, EventError> {
    Ok(read_durable_tail(tx, agent, watermark, true)?
        .into_iter()
        .map(|history| history.item)
        .collect())
}
pub fn read_compaction_tail(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
) -> Result<Vec<ConversationItem>, EventError> {
    Ok(read_durable_tail(tx, agent, watermark, false)?
        .into_iter()
        .map(|history| history.item)
        .collect())
}

pub(super) fn read_durable_tail(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
    include_compaction_statuses: bool,
) -> Result<Vec<ConversationHistoryItem>, EventError> {
    let mut read = tx.prepare("SELECT item.sequence, item.origin, item.role, item.tool_call_id, EXISTS (SELECT 1 FROM conversation_item AS status WHERE status.agent_session = item.agent_session AND status.kind = 'compaction' AND status.compaction_status_sequence = item.sequence), item.final_phase, item.provider_id, item.provider_content, item.tool_owner_sequence FROM conversation_item AS item WHERE item.agent_session = ?1 AND item.kind <> 'compaction' AND item.sequence > ?2 ORDER BY item.sequence;")?;
    let mut rows = read.query(params![agent, watermark])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let is_compaction_status = row.get(4)?;
        if is_compaction_status && !include_compaction_statuses {
            continue;
        }
        let sequence = row.get(0)?;
        items.push(ConversationHistoryItem {
            item: read_message(
                tx,
                sequence,
                &row.get::<_, String>(1)?,
                &row.get::<_, String>(2)?,
                row.get(3)?,
                row.get(5)?,
                ProviderContent {
                    provider_id: row.get(6)?,
                    content: row.get(7)?,
                },
            )?,
            tool_owner_sequence: row.get(8)?,
            is_compaction_status,
        });
    }
    Ok(items)
}

impl ConversationRepository {
    /// Reads the latest model-origin assistant response, whether or not it contains tool calls.
    pub async fn read_latest_assistant(
        &self,
        agent: String,
    ) -> Result<Option<ConversationItem>, EventError> {
        self.events
            .transaction(move |tx| {
                let sequence = tx
                    .query_row(
                        "SELECT sequence FROM conversation_item WHERE agent_session = ?1 AND origin = 'model' AND role = 'assistant' AND kind <> 'compaction' ORDER BY sequence DESC LIMIT 1;",
                        [agent],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                sequence.map(|sequence| read_conversation_item(tx, sequence)).transpose()
            })
            .await
    }

    /// Reads a flat durable tail with stored ownership and status metadata.
    pub async fn read_conversation_tail(
        &self,
        agent: String,
        after: i64,
    ) -> Result<Vec<ConversationHistoryItem>, EventError> {
        self.events
            .transaction(move |tx| read_durable_tail(tx, &agent, after, true))
            .await
    }

    /// Captures the current conversation boundary for a new public execution.
    pub async fn execution_watermark(&self, agent: String) -> Result<i64, EventError> {
        self.events.transaction(move |tx| {
            Ok(tx.query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM conversation_item WHERE agent_session = ?1;",
                [agent], |row| row.get(0),
            )?)
        }).await
    }

    /// Checks pending input or owned promoted input beyond the sealed conversation boundary.
    pub async fn has_recovery_work_after(
        &self,
        agent: String,
        watermark: i64,
    ) -> Result<bool, EventError> {
        self.events.transaction(move |tx| {
            Ok(tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM input WHERE agent_session = ?1 AND status = 'pending') OR (EXISTS (SELECT 1 FROM input WHERE agent_session = ?1 AND status = 'promoted') AND COALESCE((SELECT item.sequence > ?2 AND item.role IN ('user', 'tool') FROM conversation_item AS item WHERE item.agent_session = ?1 AND item.kind <> 'compaction' AND item.role != 'system' AND NOT EXISTS (SELECT 1 FROM conversation_item AS status WHERE status.agent_session = item.agent_session AND status.kind = 'compaction' AND status.compaction_status_sequence = item.sequence) ORDER BY item.sequence DESC LIMIT 1), 0));",
                params![agent, watermark], |row| row.get(0),
            )?)
        }).await
    }

    /// Reads one exact execution result reference without scanning its history.
    pub async fn execution_message(
        &self,
        agent: String,
        sequence: i64,
    ) -> Result<Option<ConversationItem>, EventError> {
        self.events.transaction(move |tx| {
            let owned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_item WHERE agent_session = ?1 AND kind <> 'compaction' AND sequence = ?2);",
                params![agent, sequence], |row| row.get(0),
            )?;
            if owned { read_conversation_item(tx, sequence).map(Some) } else { Ok(None) }
        }).await
    }

    /// Captures the most recent terminal assistant while its backend pass owns state.
    pub async fn latest_execution_result_sequence(
        &self,
        agent: String,
    ) -> Result<Option<i64>, EventError> {
        self.events.transaction(move |tx| {
            Ok(tx.query_row(
                "SELECT item.sequence FROM conversation_item AS item WHERE item.agent_session = ?1 AND item.role = 'assistant' AND NOT EXISTS (SELECT 1 FROM conversation_part AS call WHERE call.item_sequence = item.sequence AND call.kind = 'tool_call') ORDER BY item.sequence DESC LIMIT 1;",
                [agent], |row| row.get(0),
            ).optional()?)
        }).await
    }

    pub async fn conversation(&self, agent: String) -> Result<Vec<ConversationItem>, EventError> {
        self.conversation_after(agent, 0).await
    }
    pub async fn conversation_after(
        &self,
        agent: String,
        watermark: i64,
    ) -> Result<Vec<ConversationItem>, EventError> {
        if watermark < 0 {
            return Err(EventError::Argument(
                "watermark must not be negative".into(),
            ));
        }
        self.events
            .transaction(move |tx| read_conversation(tx, &agent, watermark))
            .await
    }
    pub async fn materialize(
        &self,
        parts: Vec<ConversationPart>,
    ) -> Result<Vec<ModelContent>, EventError> {
        let mut contents = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                ConversationPart::Text(text) => contents.push(ModelContent::Text(text)),
                ConversationPart::ImageArtifact { metadata } => {
                    let store = self.image_store.clone().ok_or_else(|| {
                        EventError::Invalid(
                            "image artifacts cannot be materialized without session resources"
                                .into(),
                        )
                    })?;
                    let width = i32::try_from(metadata.width).map_err(|_| EventError::Overflow)?;
                    let height =
                        i32::try_from(metadata.height).map_err(|_| EventError::Overflow)?;
                    let artifact_id = metadata.artifact_id;
                    let media_type = metadata.media_type;
                    let path = tokio::task::spawn_blocking(move || store.path_of(&artifact_id))
                        .await
                        .map_err(|error| EventError::from(std::io::Error::other(error)))?
                        .map_err(artifact_error)?;
                    contents.push(ModelContent::ImageFile {
                        path,
                        media_type,
                        width,
                        height,
                    });
                }
            }
        }
        Ok(contents)
    }
}
fn artifact_error(error: crate::artifacts::Error) -> EventError {
    use crate::artifacts::ErrorKind;
    let kind = match error.kind {
        ErrorKind::NotFound => std::io::ErrorKind::NotFound,
        ErrorKind::InvalidInput => std::io::ErrorKind::InvalidInput,
        ErrorKind::InvalidData => std::io::ErrorKind::InvalidData,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error).into()
}
