//! Durable compaction snapshots and their explicitly associated status message.
use super::{
    CompactionContext, CompactionSnapshot, ConversationMessage, ConversationRepository, EventError,
    MessageRole,
};
use crate::events::{projection, record, timestamp};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

fn already_saved(tx: &Transaction<'_>, agent: &str, watermark: i64) -> Result<bool, EventError> {
    Ok(tx.query_row("SELECT EXISTS (SELECT 1 FROM conversation_item WHERE agent_session = ?1 AND kind = 'compaction' AND compaction_watermark = ?2);", params![agent, watermark], |row| row.get(0))?)
}
pub(super) fn save_snapshot(
    tx: &Transaction<'_>,
    agent: &str,
    snapshot: &CompactionSnapshot,
    at: &str,
) -> Result<i64, EventError> {
    let message = &snapshot.message;
    tx.execute("INSERT INTO conversation_item (agent_session, kind, origin, role, tool_call_id, compaction_watermark, created_at, provider_id, provider_content) VALUES (?1, 'compaction', ?2, ?3, ?4, ?5, ?6, ?7, ?8);", params![agent, message.origin.as_str(), message.role.as_str(), message.tool_call_id, snapshot.watermark, at, message.provider_content.provider_id, message.provider_content.content])?;
    let sequence = tx.last_insert_rowid();
    projection::insert_message_parts(tx, sequence, message)?;
    Ok(sequence)
}
pub fn read_snapshot(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Option<CompactionSnapshot>, EventError> {
    let row = tx
        .query_row(
            "SELECT sequence, compaction_watermark, origin, role, tool_call_id, final_phase, provider_id, provider_content FROM conversation_item WHERE agent_session = ?1 AND kind = 'compaction' ORDER BY sequence DESC LIMIT 1;",
            [agent],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    super::ProviderContent {
                        provider_id: row.get(6)?,
                        content: row.get(7)?,
                    },
                ))
            },
        )
        .optional()?;
    row.map(
        |(sequence, watermark, origin, role, call, final_phase, provider)| {
            let item = super::reads::read_message(
                tx,
                sequence,
                &origin,
                &role,
                call,
                final_phase,
                provider,
            )?;
            Ok(CompactionSnapshot {
                message: item.message,
                watermark,
            })
        },
    )
    .transpose()
}
impl ConversationRepository {
    pub async fn save_compaction(
        &self,
        agent: String,
        snapshot: CompactionSnapshot,
    ) -> Result<bool, EventError> {
        self.validate(&agent)?;
        let saved_agent = agent.clone();
        let saved = self
            .events
            .transaction(move |tx| {
                if already_saved(tx, &agent, snapshot.watermark)? {
                    return Ok(false);
                }
                save_snapshot(tx, &agent, &snapshot, &timestamp())?;
                Ok(true)
            })
            .await?;
        if saved {
            self.refresh_agent_history(&saved_agent).await?;
        }
        Ok(saved)
    }
    pub async fn append_compaction_status(
        &self,
        published: Event,
        snapshot: CompactionSnapshot,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id.clone();
        let appended = self.events.transaction(move |tx| {
            if already_saved(tx, &published.agent_session_id, snapshot.watermark)? { return Ok(false); }
            let summary_sequence = save_snapshot(tx, &published.agent_session_id, &snapshot, &timestamp())?;
            record(tx, &published, &timestamp())?;
            let status = projection::project(tx, &published.agent_session_id, &ConversationMessage::text(MessageRole::System, content), &timestamp())?;
            tx.execute("UPDATE conversation_item SET compaction_status_sequence = ?1 WHERE sequence = ?2;", params![status.sequence, summary_sequence])?;
            Ok(true)
        }).await?;
        if appended {
            self.refresh_agent_history(&agent).await?;
        }
        Ok(appended)
    }
    pub async fn compaction(
        &self,
        agent: String,
    ) -> Result<Option<CompactionSnapshot>, EventError> {
        self.events
            .transaction(move |tx| read_snapshot(tx, &agent))
            .await
    }
    pub async fn compaction_history(
        &self,
        agent: String,
    ) -> Result<Option<CompactionContext>, EventError> {
        self.events.transaction(move |tx| {
            let Some(snapshot) = read_snapshot(tx, &agent)? else { return Ok(None); };
            let sequence: Option<i64> = tx.query_row("SELECT compaction_status_sequence FROM conversation_item WHERE agent_session = ?1 AND kind = 'compaction' AND compaction_watermark = ?2;", params![agent, snapshot.watermark], |row| row.get(0))?;
            let status = sequence.map(|sequence| super::reads::read_conversation_item(tx, sequence)).transpose()?;
            let tail = super::reads::read_compaction_tail(tx, &agent, snapshot.watermark)?;
            Ok(Some(CompactionContext { snapshot, status, tail }))
        }).await
    }
}
