//! Durable status/reminder boundaries. Project-only source operations do not record events.
use super::{
    ConversationItem, ConversationMessage, ConversationOrigin, ConversationRepository, EventError,
    MessageRole,
};
use crate::events::{projection, record, timestamp};
use parrot_core::StatusStanding;
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::{OptionalExtension, Transaction, params};

fn project_system(
    tx: &Transaction<'_>,
    agent: &str,
    content: String,
) -> Result<ConversationItem, EventError> {
    projection::project(
        tx,
        agent,
        &ConversationMessage::text(MessageRole::System, content),
        &timestamp(),
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
        agent: String,
        standing: StatusStanding,
        content: String,
    ) -> Result<(), EventError> {
        self.validate(&agent)?;
        let refresh_agent = agent.clone();
        let record = standing_record(&standing)?;
        self.events
            .transaction(move |tx| {
                let item = project_system(tx, &agent, content)?;
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
        agent: String,
        content: String,
    ) -> Result<(), EventError> {
        self.validate(&agent)?;
        let refresh_agent = agent.clone();
        self.events
            .transaction(move |tx| project_system(tx, &agent, content))
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
        self.events
            .transaction(move |tx| {
                record(tx, &published, &timestamp())?;
                assistant.origin = ConversationOrigin::Model;
                projection::project(tx, &published.agent_session_id, &assistant, &timestamp())?;
                project_system(tx, &published.agent_session_id, reminder)?;
                Ok(())
            })
            .await?;
        self.refresh_agent_history(&agent).await
    }
    pub async fn append_phase_prompt(
        &self,
        agent: String,
        final_phase: bool,
        content: String,
    ) -> Result<(), EventError> {
        self.validate(&agent)?;
        let refresh_agent = agent.clone();
        self.events
            .transaction(move |tx| {
                let item = project_system(tx, &agent, content)?;
                tx.execute(
                    "UPDATE conversation_item SET final_phase = ?1 WHERE sequence = ?2;",
                    params![final_phase, item.sequence],
                )?;
                Ok(())
            })
            .await?;
        self.refresh_agent_history(&refresh_agent).await
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
