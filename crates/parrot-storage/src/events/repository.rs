//! Async repository facade; SQL and validation stay on the session actor.
use super::*;
use parrot_domain::protocol::messages::parrot::{Delivery, RequestUsageRecorded};

pub type InputTicket<T> = CommitTicket<(String, String), T, EventError>;

impl EventRepository {
    /// Admission keys retain both source agent and sender message identity.
    pub fn admit_input<F>(
        &self,
        agent: String,
        message: String,
        parts: Vec<ConversationPart>,
        delivery: Delivery,
        compose: F,
        cancellation: &dyn Cancellation,
    ) -> Result<InputTicket<Admission>, AdmissionError<EventError>>
    where
        F: FnOnce(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        let context = self.context().clone();
        self.admit((agent.clone(), message.clone()), cancellation, move |tx| {
            inputs::admit_parts(tx, &context, &agent, &message, parts, delivery, compose)
        })
    }
    pub fn admit_steer_if_idle<F>(
        &self,
        agent: String,
        message: String,
        content: String,
        compose: F,
        cancellation: &dyn Cancellation,
    ) -> Result<InputTicket<Option<Admission>>, AdmissionError<EventError>>
    where
        F: FnOnce(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        let context = self.context().clone();
        self.admit((agent.clone(), message.clone()), cancellation, move |tx| {
            inputs::admit_steer_if_idle(tx, &context, &agent, &message, content, compose)
        })
    }
    pub async fn cancel_pending_input<F>(
        &self,
        agent: String,
        input: String,
        compose: F,
    ) -> Result<Option<Event>, EventError>
    where
        F: FnOnce() -> Result<Event, EventError> + Send + 'static,
    {
        let context = self.context().clone();
        self.transaction(move |tx| inputs::cancel_pending(tx, &context, &agent, &input, compose))
            .await
    }
    pub async fn promote_steers<F>(
        &self,
        agent: String,
        compose: F,
    ) -> Result<Vec<Promotion>, EventError>
    where
        F: FnMut(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        let context = self.context().clone();
        self.transaction(move |tx| {
            inputs::promote(tx, &context, &agent, Delivery::DELIVERY_STEER, -1, compose)
        })
        .await
    }
    pub async fn promote_next_queue<F>(
        &self,
        agent: String,
        compose: F,
    ) -> Result<Vec<Promotion>, EventError>
    where
        F: FnMut(&AdmittedInput) -> Result<Event, EventError> + Send + 'static,
    {
        let context = self.context().clone();
        self.transaction(move |tx| {
            inputs::promote(tx, &context, &agent, Delivery::DELIVERY_QUEUE, 1, compose)
        })
        .await
    }
    pub async fn has_pending_inputs(&self, agent: String) -> Result<bool, EventError> {
        self.transaction(move |tx| inputs::has_pending(tx, &agent))
            .await
    }
    pub async fn inputs_for_next_promotion(
        &self,
        agent: String,
    ) -> Result<Vec<AdmittedInput>, EventError> {
        self.transaction(move |tx| inputs::inputs_for_next_promotion(tx, &agent))
            .await
    }

    /// Source ordinary append: duplicate event IDs remain errors, not upserts.
    pub fn admit_append(
        &self,
        event: Event,
        raw: Option<Vec<u8>>,
        message: Option<ConversationMessage>,
        cancellation: &dyn Cancellation,
    ) -> Result<CommitTicket<String, AppendOutcome, EventError>, AdmissionError<EventError>> {
        let context = self.context().clone();
        self.admit(event.id.clone(), cancellation, move |tx| {
            append(tx, &context, &event, raw.as_deref(), message.as_ref())
        })
    }
    pub async fn append(
        &self,
        event: Event,
        message: Option<ConversationMessage>,
    ) -> Result<AppendOutcome, EventError> {
        let ticket = self
            .admit_append(event, None, message, &NeverCancelled)
            .map_err(rejected)?;
        Ok(ticket.into_completion().await?.into_value().into_output())
    }
    pub async fn append_raw(
        &self,
        raw: Vec<u8>,
        message: Option<ConversationMessage>,
    ) -> Result<AppendOutcome, EventError> {
        let event = Event::parse_from_bytes(&raw)?;
        let ticket = self
            .admit_append(event, Some(raw), message, &NeverCancelled)
            .map_err(rejected)?;
        Ok(ticket.into_completion().await?.into_value().into_output())
    }
    pub async fn usage(&self) -> Result<SessionUsage, EventError> {
        self.transaction(|tx| {
            statistics::ensure_usage_projection(tx)?;
            statistics::capture_usage(tx)
        })
        .await
    }
    pub async fn append_usage_fact(&self, event: Event) -> Result<i64, EventError> {
        let context = self.context().clone();
        self.transaction(move |tx| {
            statistics::append_usage_fact(tx, &event, context.timestamp.as_ref())
        })
        .await
    }
    pub async fn find_request_usage(
        &self,
        agent: String,
        call: String,
    ) -> Result<Option<RequestUsageRecorded>, EventError> {
        self.transaction(move |tx| statistics::find_request_usage(tx, &agent, &call))
            .await
    }
    pub async fn agent_lineage(&self) -> Result<Vec<AgentLineageRecord>, EventError> {
        self.transaction(statistics::agent_lineage).await
    }
    pub async fn replay_statistics(
        &self,
    ) -> Result<crate::diagnostics::statistics::AgentStatisticsReplay, EventError> {
        self.transaction(statistics::replay_statistics).await
    }
    pub async fn append_tool_terminal(
        &self,
        event: Event,
        terminal: ToolExecutionTerminal,
    ) -> Result<bool, EventError> {
        let context = self.context().clone();
        self.transaction(move |tx| {
            tools::append_tool_terminal(tx, &event, &terminal, context.timestamp.as_ref())
        })
        .await
    }
    pub async fn append_tool_settlement(
        &self,
        event: Event,
        assistant_sequence: i64,
        terminal: ToolExecutionTerminal,
    ) -> Result<Option<ConversationItem>, EventError> {
        let context = self.context().clone();
        self.transaction(move |tx| {
            tools::append_tool_settlement(
                tx,
                &event,
                assistant_sequence,
                &terminal,
                context.timestamp.as_ref(),
            )
        })
        .await
    }
    pub async fn append_tool_synthetic(
        &self,
        event: Event,
        assistant_sequence: i64,
        parts: Vec<ConversationPart>,
    ) -> Result<Option<ConversationItem>, EventError> {
        let context = self.context().clone();
        self.transaction(move |tx| {
            tools::append_tool_synthetic(
                tx,
                &event,
                assistant_sequence,
                &parts,
                context.timestamp.as_ref(),
            )
        })
        .await
    }
    pub async fn has_tool_synthetic(
        &self,
        agent: String,
        assistant_sequence: i64,
    ) -> Result<bool, EventError> {
        self.transaction(move |tx| tools::has_tool_synthetic(tx, &agent, assistant_sequence))
            .await
    }
    pub async fn tool_terminals(
        &self,
        agent: String,
    ) -> Result<Vec<ToolExecutionTerminal>, EventError> {
        self.transaction(move |tx| tools::tool_terminals(tx, &agent))
            .await
    }
    pub async fn tool_terminals_of_calls(
        &self,
        agent: String,
        calls: Vec<String>,
    ) -> Result<Vec<ToolExecutionTerminal>, EventError> {
        self.transaction(move |tx| tools::tool_terminals_of_calls(tx, &agent, &calls))
            .await
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppendOutcome {
    pub stored: StoredEvent,
    pub conversation: Option<ConversationItem>,
    pub usage: Option<SessionUsage>,
}
pub fn append(
    tx: &Transaction<'_>,
    context: &RepositoryContext,
    event: &Event,
    raw: Option<&[u8]>,
    message: Option<&ConversationMessage>,
) -> Result<AppendOutcome, EventError> {
    if event.has_agent_statistics_updated() {
        statistics::ensure_usage_projection(tx)?;
    }
    let stored = record(tx, event, raw, &(context.timestamp)())?;
    let conversation = message
        .map(|message| {
            projection::project(tx, &event.agent_session_id, message, &(context.timestamp)())
        })
        .transpose()?;
    let usage = if event.has_agent_statistics_updated() {
        statistics::project_usage(
            tx,
            &event.agent_session_id,
            stored.sequence,
            event.agent_statistics_updated(),
        )?;
        Some(statistics::capture_usage(tx)?)
    } else {
        None
    };
    Ok(AppendOutcome {
        stored,
        conversation,
        usage,
    })
}
fn rejected(error: AdmissionError<EventError>) -> EventError {
    match error {
        AdmissionError::Rejected(error) => error,
        AdmissionError::Cancelled => unreachable!("uncancelled internal operation"),
    }
}
