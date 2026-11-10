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
    pub async fn read_final_phase(&self, agent: String) -> Result<bool, EventError> {
        self.events
            .transaction(move |tx| projection::read_latest_final_phase(tx, &agent))
            .await
    }
    pub async fn pending_status(&self, agent: String) -> Result<Option<PendingStatus>, EventError> {
        loop {
            let state = self
                .metadata
                .read_session_state()
                .map_err(|error| EventError::Invalid(error.to_string()))?;
            if agent != state.root_agent_session_id {
                return Ok(None);
            }
            let Some(guard) = self
                .metadata
                .guard_status(&agent, &state.mode)
                .await
                .map_err(|error| EventError::Invalid(error.to_string()))?
            else {
                continue;
            };
            let read_agent = agent.clone();
            let (consumed, guard) = self.events.transaction(move |tx| {
                let consumed: Option<i64> = tx.query_row(
                    "SELECT MAX(mode_status_revision) FROM conversation_item WHERE agent_session = ?1 AND mode_status_revision IS NOT NULL;",
                    [read_agent],
                    |row| row.get(0),
                )?;
                Ok((consumed, guard))
            }).await?;
            drop(guard);
            return match consumed {
                Some(consumed) if consumed > state.mode.version => Err(EventError::Invalid(
                    "consumed mode revision exceeds session metadata revision".into(),
                )),
                Some(consumed) if consumed == state.mode.version => Ok(None),
                _ => Ok(Some(state.mode)),
            };
        }
    }
    pub async fn append_status_prompt(
        &self,
        published: Event,
        expected: PendingStatus,
        content: String,
    ) -> Result<bool, EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let Some(guard) = self
            .metadata
            .guard_status(&agent, &expected)
            .await
            .map_err(|error| EventError::Invalid(error.to_string()))?
        else {
            return Ok(false);
        };
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let (appended, guard) = self.events.transaction(move |tx| {
            let consumed: Option<i64> = tx.query_row(
                "SELECT MAX(mode_status_revision) FROM conversation_item WHERE agent_session = ?1 AND mode_status_revision IS NOT NULL;",
                [&agent],
                |row| row.get(0),
            )?;
            if consumed.is_some_and(|revision| revision > expected.version) {
                return Err(EventError::Invalid(
                    "consumed mode revision exceeds session metadata revision".into(),
                ));
            }
            if consumed == Some(expected.version) {
                return Ok((false, guard));
            }
            let item = project_system(tx, &context, &agent, content)?;
            tx.execute(
                "UPDATE conversation_item SET mode_status_revision = ?1 WHERE sequence = ?2;",
                params![expected.version, item.sequence],
            )?;
            Ok((true, guard))
        }).await?;
        drop(guard);
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
