//! Durable status/reminder boundaries. Project-only source operations do not record events.
use super::{
    ContextReminderCheckpoint, ConversationMessage, ConversationOrigin, ConversationRepository,
    EventError, MessageRole, PendingStatus,
};
use crate::events::{RepositoryContext, projection, record};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

fn project_system(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    agent: &str,
    content: String,
) -> Result<(), EventError> {
    projection::project(
        tx,
        agent,
        &ConversationMessage::text(MessageRole::System, content),
        &(context.timestamp)(),
    )?;
    Ok(())
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
    pub async fn update_mode(
        &self,
        user_session: String,
        agent: String,
        mode: String,
    ) -> Result<(), EventError> {
        let context = self.events.context().clone();
        self.events.transaction(move |tx| {
            tx.execute("UPDATE session_state SET mode = ?1 WHERE user_session = ?2;", params![mode, user_session])?;
            tx.execute("INSERT INTO mode_change (agent_session, mode, created_at) VALUES (?1, ?2, ?3);", params![agent, mode, (context.timestamp)()])?;
            Ok(())
        }).await
    }
    pub async fn pending_status(&self, agent: String) -> Result<Option<PendingStatus>, EventError> {
        self.events.transaction(move |tx| {
            let row: Option<(String, i64, i64)> = tx.query_row("SELECT state.mode, COALESCE((SELECT MAX(sequence) FROM mode_change WHERE agent_session = ?1), -1) AS version, COALESCE((SELECT MAX(mode_change_sequence) FROM status_prompt WHERE agent_session = ?1), -2) AS consumed FROM session_state AS state WHERE state.agent_session = ?1;", [&agent], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
            Ok(row.and_then(|(mode,version,consumed)| (version > consumed).then_some(PendingStatus {mode,version})))
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
            let pending: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM session_state WHERE agent_session = ?1 AND mode = ?2) AND COALESCE((SELECT MAX(sequence) FROM mode_change WHERE agent_session = ?1), -1) = ?3 AND ?3 > COALESCE((SELECT MAX(mode_change_sequence) FROM status_prompt WHERE agent_session = ?1), -2);", params![agent, expected.mode, expected.version], |row| row.get(0))?;
            if !pending { return Ok(false); }
            project_system(tx, &context, &agent, content)?;
            tx.execute("INSERT INTO status_prompt (agent_session, mode_change_sequence, created_at) VALUES (?1, ?2, ?3);", params![agent, expected.version, (context.timestamp)()])?;
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
            Ok(tx.query_row("SELECT canonical_model, context_limit, percentage FROM context_reminder WHERE agent_session = ?1 ORDER BY sequence DESC LIMIT 1;", [agent], |row| Ok(ContextReminderCheckpoint { canonical_model: row.get(0)?, context_limit: row.get(1)?, percentage: row.get(2)? })).optional()?)
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
            let previous: Option<i32> = tx.query_row("SELECT percentage FROM context_reminder WHERE agent_session = ?1 AND canonical_model = ?2 AND context_limit = ?3 ORDER BY sequence DESC LIMIT 1;", params![agent, checkpoint.canonical_model, checkpoint.context_limit], |row| row.get(0)).optional()?;
            if previous.is_some_and(|previous| previous >= checkpoint.percentage) { return Ok(false); }
            tx.execute("INSERT INTO context_reminder (agent_session, canonical_model, context_limit, percentage, created_at) VALUES (?1, ?2, ?3, ?4, ?5);", params![agent, checkpoint.canonical_model, checkpoint.context_limit, checkpoint.percentage, (context.timestamp)()])?;
            project_system(tx, &context, &agent, content)?;
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
    ) -> Result<(), EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        self.events.transaction(move |tx| {
            tx.execute("INSERT INTO final_provider_request_prompt (agent_session, created_at) VALUES (?1, ?2);", params![agent, (context.timestamp)()])?;
            project_system(tx, &context, &agent, content)
        }).await?;
        self.refresh_agent_history(&refresh_agent).await
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
                if tx.execute(
                    "DELETE FROM final_provider_request_prompt WHERE agent_session = ?1;",
                    [&agent],
                )? == 0
                {
                    return Ok(false);
                }
                project_system(tx, &context, &agent, content)?;
                Ok(true)
            })
            .await?;
        if appended {
            self.refresh_agent_history(&refresh_agent).await?;
        }
        Ok(appended)
    }
}
