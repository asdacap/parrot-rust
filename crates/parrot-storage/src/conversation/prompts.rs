//! Durable status/reminder boundaries. Project-only source operations do not record events.
use super::{
    ContextReminderCheckpoint, ConversationItem, ConversationMessage, ConversationOrigin,
    ConversationRepository, EventError, MessageRole,
};
use crate::events::{RepositoryContext, projection, record};
use parrot_core::StatusStanding;
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
    /// Reads the slot hashes standing from the latest persisted status assembly
    /// for the agent, or `None` while no assembly exists. Superseded assemblies
    /// remain in history; only the newest record stands.
    pub async fn standing_statuses(
        &self,
        agent: String,
    ) -> Result<Option<StatusStanding>, EventError> {
        self.events.transaction(move |tx| {
            let record: Option<String> = tx
                .query_row(
                    "SELECT status_slots FROM conversation_item WHERE agent_session = ?1 AND status_slots IS NOT NULL ORDER BY sequence DESC LIMIT 1;",
                    [agent],
                    |row| row.get(0),
                )
                .optional()?;
            record
                .map(|record| parse_standing(&record))
                .transpose()
        })
        .await
    }
    /// Appends one status assembly as a system row carrying the standing slot
    /// hashes. Appends never rewrite or suppress earlier assemblies.
    pub async fn append_status_assembly(
        &self,
        published: Event,
        standing: StatusStanding,
        content: String,
    ) -> Result<(), EventError> {
        self.validate(&published.agent_session_id)?;
        let agent = published.agent_session_id;
        let refresh_agent = agent.clone();
        let context = self.events.context().clone();
        let record = standing_record(&standing)?;
        self.events
            .transaction(move |tx| {
                let item = project_system(tx, &context, &agent, content)?;
                tx.execute(
                    "UPDATE conversation_item SET status_slots = ?1 WHERE sequence = ?2;",
                    params![record, item.sequence],
                )?;
                Ok(())
            })
            .await?;
        self.refresh_agent_history(&refresh_agent).await
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

fn standing_record(standing: &StatusStanding) -> Result<String, EventError> {
    let record: std::collections::BTreeMap<&str, &str> = standing
        .iter()
        .map(|(slot, hash)| (slot.as_str(), hash.as_str()))
        .collect();
    serde_json::to_string(&record).map_err(|error| EventError::Invalid(error.to_string()))
}

fn parse_standing(record: &str) -> Result<StatusStanding, EventError> {
    let parsed: std::collections::BTreeMap<String, String> =
        serde_json::from_str(record).map_err(|error| EventError::Invalid(error.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|(slot, hash)| (parrot_core::StatusKey::new(slot), hash))
        .collect())
}
