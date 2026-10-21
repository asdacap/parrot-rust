//! Structured reads use the same selected-session transaction as projection writes.
use super::{ConversationRepository, EventError, types::*};
use crate::database::Transaction;
use crate::events::projection::{ensure_conversation_projection, read_parts};
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
) -> Result<ConversationItem, EventError> {
    let origin = parse_origin(origin)?;
    let role = parse_role(role)?;
    let parts = read_parts(tx, "conversation_part", "item_sequence", &sequence)?;
    let mut calls = tx.prepare("SELECT id, name, arguments FROM conversation_tool_call WHERE item_sequence = ?1 ORDER BY position;")?;
    let tool_calls = calls
        .query_map([sequence], |row| {
            Ok(ToolCall {
                id: row.get(0)?,
                name: row.get(1)?,
                arguments_json: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let provider_content = tx
        .query_row(
            "SELECT provider, content FROM conversation_provider_content WHERE item_sequence = ?1;",
            [sequence],
            |row| {
                Ok(ProviderContent {
                    provider_id: row.get(0)?,
                    content: row.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default();
    Ok(ConversationItem {
        sequence,
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
            "SELECT origin, role, tool_call_id FROM conversation_item WHERE sequence = ?1;",
            [sequence],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let (origin, role, call) = item.ok_or_else(|| {
        EventError::Invalid(format!("Conversation item {sequence} does not exist."))
    })?;
    read_message(tx, sequence, &origin, &role, call)
}
fn read_selected(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
    tail: bool,
) -> Result<Vec<ConversationItem>, EventError> {
    let sql = if tail {
        "SELECT item.sequence, item.origin, item.role, item.tool_call_id FROM conversation_item AS item WHERE item.agent_session = ?1 AND item.sequence > ?2 AND NOT EXISTS (SELECT 1 FROM compaction_status AS status WHERE status.agent_session = item.agent_session AND status.status_conversation_sequence = item.sequence) ORDER BY item.sequence;"
    } else {
        "SELECT sequence, origin, role, tool_call_id FROM conversation_item WHERE agent_session = ?1 AND sequence > ?2 ORDER BY sequence;"
    };
    let mut read = tx.prepare(sql)?;
    let mut rows = read.query(params![agent, watermark])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(read_message(
            tx,
            row.get(0)?,
            &row.get::<_, String>(1)?,
            &row.get::<_, String>(2)?,
            row.get(3)?,
        )?);
    }
    Ok(items)
}
pub fn read_conversation(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
) -> Result<Vec<ConversationItem>, EventError> {
    read_selected(tx, agent, watermark, false)
}
pub fn read_compaction_tail(
    tx: &Transaction<'_>,
    agent: &str,
    watermark: i64,
) -> Result<Vec<ConversationItem>, EventError> {
    read_selected(tx, agent, watermark, true)
}

impl ConversationRepository {
    pub async fn resolve_image_artifact(
        &self,
        artifact_id: String,
    ) -> Result<Option<crate::artifacts::ImageArtifactMetadata>, EventError> {
        if artifact_id.trim().is_empty() {
            return Err(EventError::Argument(
                "artifactId must not be empty or whitespace".into(),
            ));
        }
        self.events.transaction(move |tx| Ok(tx.query_row("SELECT artifact.artifact_id, content.sha256, content.media_type, content.byte_length, content.width, content.height, content.frame_count, content.aggregate_pixels, artifact.display_name, artifact.origin FROM image_artifact AS artifact JOIN image_content AS content ON content.sha256 = artifact.sha256 WHERE artifact.artifact_id = ?1;", [artifact_id], |row| Ok(crate::artifacts::ImageArtifactMetadata {
            artifact_id: row.get(0)?, sha256: row.get(1)?, media_type: row.get(2)?, byte_length: row.get(3)?, width: row.get(4)?, height: row.get(5)?, frame_count: row.get(6)?, aggregate_pixels: row.get(7)?, display_name: row.get(8)?, origin: row.get(9)?,
        })).optional()?)).await
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
            .transaction(move |tx| {
                ensure_conversation_projection(tx)?;
                read_conversation(tx, &agent, watermark)
            })
            .await
    }
    /// Source legacy model reads reject artifact references; only materialize resolves them.
    pub async fn model_history(&self, agent: String) -> Result<Vec<ModelMessage>, EventError> {
        self.conversation(agent)
            .await?
            .into_iter()
            .map(|item| {
                let message = item.message;
                if message
                    .parts
                    .iter()
                    .any(|part| matches!(part, ConversationPart::ImageArtifact { .. }))
                {
                    return Err(EventError::Invalid(
                        "artifact references must be materialized before model history is read"
                            .into(),
                    ));
                }
                Ok(ModelMessage {
                    role: message.role,
                    contents: message
                        .parts
                        .into_iter()
                        .map(|part| match part {
                            ConversationPart::Text(text) => ModelContent::Text(text),
                            _ => unreachable!("checked artifact references"),
                        })
                        .collect(),
                    tool_calls: message.tool_calls,
                    tool_call_id: message.tool_call_id,
                    provider_content: message.provider_content,
                })
            })
            .collect()
    }
    pub async fn messages(&self, agent: String) -> Result<Vec<String>, EventError> {
        Ok(self
            .model_history(agent)
            .await?
            .into_iter()
            .map(|message| format!("{}: {}", message.role.as_str(), message.content()))
            .collect())
    }
    pub async fn materialize(
        &self,
        parts: Vec<ConversationPart>,
    ) -> Result<Vec<ModelContent>, EventError> {
        let mut contents = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                ConversationPart::Text(text) => contents.push(ModelContent::Text(text)),
                ConversationPart::ImageArtifact {
                    artifact_id,
                    media_type,
                    ..
                } => {
                    let store = self.image_store.clone().ok_or_else(|| {
                        EventError::Invalid(
                            "image artifacts cannot be materialized without session resources"
                                .into(),
                        )
                    })?;
                    let artifact = self
                        .resolve_image_artifact(artifact_id.clone())
                        .await?
                        .ok_or_else(|| {
                            EventError::from(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "The image artifact does not exist.",
                            ))
                        })?;
                    let width = i32::try_from(artifact.width).map_err(|_| EventError::Overflow)?;
                    let height =
                        i32::try_from(artifact.height).map_err(|_| EventError::Overflow)?;
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
