//! Durable status/reminder boundaries. Project-only source operations do not record events.
use super::{
    ContextReminderCheckpoint, ConversationItem, ConversationMessage, ConversationOrigin,
    ConversationRepository, EventError, MessageRole, PendingStatus,
};
use crate::events::{RepositoryContext, projection, record};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

fn project_system(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    content: String,
) -> Result<ConversationItem, EventError> {
    projection::project(
        tx,
        agent,
        &ConversationMessage::text(MessageRole::System, content),
        &(context.timestamp)(),
    )
}
impl ConversationRepository {
    pub async fn session_state(
        &self,
        user_session: String,
        requested_mode: String,
    ) -> Result<(String, String), EventError> {
        let generate = self.agent_session_id.clone();
        self.events.transaction(move |tx| {
            let agent = generate()?;
            tx.execute("INSERT OR IGNORE INTO session_state (user_session, agent_session, mode) VALUES (?1, ?2, ?3);", params![user_session, agent, requested_mode])?;
            Ok(tx.query_row("SELECT agent_session, mode FROM session_state WHERE user_session = ?1;", [user_session], |row| Ok((row.get(0)?, row.get(1)?)))?)
        }).await
    }
    pub async fn update_mode(&self, user_session: String, mode: String) -> Result<(), EventError> {
        self.events.transaction(move |tx| {
            let revision: Option<i64> = tx.query_row("SELECT mode_revision FROM session_state WHERE user_session = ?1;", [&user_session], |row| row.get(0)).optional()?;
            let revision = revision.ok_or_else(|| EventError::Invalid("session state does not exist".into()))?
                .checked_add(1).ok_or_else(|| EventError::Invalid("mode revision overflow".into()))?;
            tx.execute("UPDATE session_state SET mode = ?1, mode_revision = ?2 WHERE user_session = ?3;", params![mode, revision, user_session])?;
            Ok(())
        }).await
    }
    pub async fn pending_status(&self, agent: String) -> Result<Option<PendingStatus>, EventError> {
        self.events.transaction(move |tx| {
            Ok(tx.query_row("SELECT mode, mode_revision FROM session_state WHERE agent_session = ?1 AND (status_revision IS NULL OR mode_revision > status_revision);", [agent], |row| Ok(PendingStatus { mode: row.get(0)?, version: row.get(1)? })).optional()?)
        }).await
    }
    pub async fn append_status_prompt(
        &self,
        published: Event,
        expected: PendingStatus,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let appended = self.events.transaction(move |tx| {
            let pending: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM session_state WHERE agent_session = ?1 AND mode = ?2 AND mode_revision = ?3 AND (status_revision IS NULL OR mode_revision > status_revision));", params![agent, expected.mode, expected.version], |row| row.get(0))?;
            if !pending { return Ok(false); }
            project_system(tx, &context, &agent, content)?;
            tx.execute("UPDATE session_state SET status_revision = ?1 WHERE agent_session = ?2;", params![expected.version, agent])?;
            Ok(true)
        }).await?;
        if appended {
            self.refresh_agent_history(&refresh_agent).await?;
        }
        Ok(appended)
    }
    pub async fn append_system_prompt(
        &self,
        published: Event,
        content: String,
    ) -> Result<(), EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        self.events
            .transaction(move |tx| project_system(tx, &context, &agent, content))
            .await?;
        self.refresh_agent_history(&refresh_agent).await
    }
    pub async fn append_retained_reminder(
        &self,
        published: Event,
        mut assistant: ConversationMessage,
        reminder: String,
    ) -> Result<(), EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id.clone();
        let context = self.events.context().clone();
        self.events
            .transaction(move |tx| {
                record(tx, &published, None, &(context.timestamp)())?;
                assistant.origin = ConversationOrigin::Model;
                projection::project(
                    tx,
                    &published.agent_session_id,
                    &assistant,
                    &(context.timestamp)(),
                )?;
                project_system(tx, &context, &published.agent_session_id, reminder)?;
                Ok(())
            })
            .await?;
        self.refresh_agent_history(&agent).await
    }
    pub async fn latest_context_reminder(
        &self,
        agent: String,
    ) -> Result<Option<ContextReminderCheckpoint>, EventError> {
        if agent.trim().is_empty() {
            return Err(EventError::Argument(
                "agentSessionId must not be empty or whitespace".into(),
            ));
        }
        self.events.transaction(move |tx| {
            Ok(tx.query_row("SELECT json_extract(context_reminder, '$.canonical_model'), json_extract(context_reminder, '$.context_limit'), json_extract(context_reminder, '$.percentage') FROM conversation_item WHERE agent_session = ?1 AND context_reminder IS NOT NULL ORDER BY sequence DESC LIMIT 1;", [agent], |row| Ok(ContextReminderCheckpoint { canonical_model: row.get(0)?, context_limit: row.get(1)?, percentage: row.get(2)? })).optional()?)
        }).await
    }
    pub async fn append_context_reminder(
        &self,
        published: Event,
        checkpoint: ContextReminderCheckpoint,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let appended = self.events.transaction(move |tx| {
            let previous: Option<i32> = tx.query_row("SELECT json_extract(context_reminder, '$.percentage') FROM conversation_item WHERE agent_session = ?1 AND context_reminder IS NOT NULL AND json_extract(context_reminder, '$.canonical_model') = ?2 AND json_extract(context_reminder, '$.context_limit') = ?3 ORDER BY sequence DESC LIMIT 1;", params![agent, checkpoint.canonical_model, checkpoint.context_limit], |row| row.get(0)).optional()?;
            if previous.is_some_and(|previous| previous >= checkpoint.percentage) { return Ok(false); }
            let tag = serde_json::to_string(&checkpoint).map_err(|error| EventError::Invalid(error.to_string()))?;
            let item = project_system(tx, &context, &agent, content)?;
            tx.execute("UPDATE conversation_item SET kind = 'context_reminder', context_reminder = ?1 WHERE sequence = ?2;", params![tag, item.sequence])?;
            Ok(true)
        }).await?;
        if appended {
            self.refresh_agent_history(&refresh_agent).await?;
        }
        Ok(appended)
    }
    pub async fn append_final_provider_request_prompt(
        &self,
        published: Event,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let appended = self
            .events
            .transaction(move |tx| {
                if projection::read_latest_final_phase(tx, &agent)? {
                    return Ok(false);
                }
                let item = project_system(tx, &context, &agent, content)?;
                tx.execute(
                    "UPDATE conversation_item SET final_phase = 1 WHERE sequence = ?1;",
                    [item.sequence],
                )?;
                Ok(true)
            })
            .await?;
        if appended {
            self.refresh_agent_history(&refresh_agent).await?;
        }
        Ok(appended)
    }
    pub async fn append_tool_availability_restored_prompt(
        &self,
        published: Event,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let appended = self
            .events
            .transaction(move |tx| {
                if !projection::read_latest_final_phase(tx, &agent)? {
                    return Ok(false);
                }
                let item = project_system(tx, &context, &agent, content)?;
                tx.execute(
                    "UPDATE conversation_item SET final_phase = 0 WHERE sequence = ?1;",
                    [item.sequence],
                )?;
                Ok(true)
            })
            .await?;
        if appended {
            self.refresh_agent_history(&refresh_agent).await?;
        }
        Ok(appended)
    }
}
