//! Durable status/reminder boundaries. Project-only source operations do not record events.
use super::{
    ConversationItem, ConversationMessage, ConversationOrigin, ConversationRepository, EventError,
    MessageRole,
};
use crate::events::{projection, record, timestamp};
use parrot_domain::protocol::messages::parrot::Event;
use rusqlite::Transaction;

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
}
