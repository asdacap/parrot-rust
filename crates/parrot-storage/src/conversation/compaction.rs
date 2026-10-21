//! Durable compaction snapshots and their explicitly associated status message.
use super::{
    CompactionContext, CompactionSnapshot, ConversationMessage, ConversationRepository, EventError,
    MessageRole,
};
use crate::events::{projection, record};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

fn already_saved(tx: &Transaction<'_>, agent: &str, watermark: i64) -> Result<bool, EventError> {
    Ok(tx.query_row("SELECT EXISTS (SELECT 1 FROM agent_history WHERE agent_session = ?1 AND kind = 'compaction' AND watermark = ?2);", params![agent, watermark], |row| row.get(0))?)
}
fn save_snapshot(
    tx: &Transaction<'_>,
    agent: &str,
    snapshot: &CompactionSnapshot,
    at: &str,
) -> Result<(), EventError> {
    tx.execute("INSERT INTO compaction_snapshot (agent_session, summary, watermark, created_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (agent_session) DO UPDATE SET summary = excluded.summary, watermark = excluded.watermark, created_at = excluded.created_at;", params![agent, snapshot.summary, snapshot.watermark, at])?;
    tx.execute("INSERT INTO agent_history (agent_session, kind, conversation_sequence, summary, watermark, created_at) VALUES (?1, 'compaction', NULL, ?2, ?3, ?4);", params![agent, snapshot.summary, snapshot.watermark, at])?;
    Ok(())
}
pub fn read_snapshot(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Option<CompactionSnapshot>, EventError> {
    Ok(tx
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
        .optional()?)
}
impl ConversationRepository {
    pub async fn save_compaction(
        &self,
        agent: String,
        snapshot: CompactionSnapshot,
    ) -> Result<bool, EventError> {
        self.validate(&agent)?;
        let saved_agent = agent.clone();
        let context = self.events.context().clone();
        let saved = self
            .events
            .transaction(move |tx| {
                projection::ensure_agent_history_projection(tx)?;
                if already_saved(tx, &agent, snapshot.watermark)? {
                    return Ok(false);
                }
                save_snapshot(tx, &agent, &snapshot, &(context.timestamp)())?;
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
        let context = self.events.context().clone();
        let appended = self.events.transaction(move |tx| {
            projection::ensure_agent_history_projection(tx)?;
            if already_saved(tx, &published.agent_session_id, snapshot.watermark)? { return Ok(false); }
            save_snapshot(tx, &published.agent_session_id, &snapshot, &(context.timestamp)())?;
            record(tx, &published, None, &(context.timestamp)())?;
            let status = projection::project(tx, &published.agent_session_id, &ConversationMessage::text(MessageRole::System, content), &(context.timestamp)())?;
            tx.execute("INSERT INTO compaction_status (agent_session, watermark, status_conversation_sequence) VALUES (?1, ?2, ?3);", params![published.agent_session_id, snapshot.watermark, status.sequence])?;
            tx.execute("DELETE FROM context_reminder WHERE agent_session = ?1;", [&published.agent_session_id])?;
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
            projection::ensure_conversation_projection(tx)?;
            let Some(snapshot) = read_snapshot(tx, &agent)? else { return Ok(None); };
            let sequence: Option<i64> = tx.query_row("SELECT status_conversation_sequence FROM compaction_status WHERE agent_session = ?1 AND watermark = ?2;", params![agent, snapshot.watermark], |row| row.get(0)).optional()?;
            let status = sequence.map(|sequence| super::reads::read_conversation_item(tx, sequence)).transpose()?;
            let tail = super::reads::read_compaction_tail(tx, &agent, snapshot.watermark)?;
            Ok(Some(CompactionContext { snapshot, status, tail }))
        }).await
    }
}
