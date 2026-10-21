use crate::event_broker::EventBroker;
use parrot_domain::{
    protocol::messages::parrot::{
        Event, RequestUsageRecorded, SessionUsageSnapshot, ToolExecutionStarted,
    },
    values::{
        accounting::{
            AgentCompletionUsage, AgentSessionStatistics, AgentSessionStatisticsSnapshot,
            AgentUsageIncrement, AgentUsageKey, AgentUsagePricing,
        },
        identifiers::Identifier,
    },
};
use parrot_provider::{
    catalogs::ModelMetadataFields,
    routing::registry::ProviderModel,
    sessions::{Completion, Reasoning, ToolCall},
};
use parrot_storage::{
    conversation::AgentHistoryView, diagnostics::statistics::UserSessionStatistics,
    events::EventError,
};
use std::sync::Arc;

/// Session-scoped durable usage recording with live, immediate-parent-to-root
/// statistics handles supplied by the session owner.
pub struct SessionUsageRecorder {
    history: AgentHistoryView,
    agent_session_id: String,
    parent_agent_session_id: String,
    broker: Arc<EventBroker>,
    coordinator: Arc<UserSessionStatistics>,
    statistics: Arc<AgentSessionStatistics>,
    root: Arc<AgentSessionStatistics>,
    ancestors: Vec<Arc<AgentSessionStatistics>>,
}

impl SessionUsageRecorder {
    pub async fn new(
        history: AgentHistoryView,
        parent_agent_session_id: String,
        broker: Arc<EventBroker>,
        ancestors: Vec<(String, Arc<AgentSessionStatistics>)>,
    ) -> Result<Self, EventError> {
        let agent_session_id = history.agent_session_id().to_owned();
        if ancestors.first().map_or("", |(id, _)| id.as_str()) != parent_agent_session_id {
            return Err(EventError::Argument(
                "usage ancestors must begin with the immediate parent".into(),
            ));
        }
        let coordinator = history
            .repository()
            .events()
            .get_runtime_statistics()
            .await?;
        for (index, (id, statistics)) in ancestors.iter().enumerate() {
            if id.is_empty()
                || id == &agent_session_id
                || ancestors[..index]
                    .iter()
                    .any(|(previous, _)| previous == id)
                || !Arc::ptr_eq(statistics, &coordinator.get_agent_statistics(id))
            {
                return Err(EventError::Argument(
                    "usage ancestors must be distinct authoritative live statistics handles".into(),
                ));
            }
        }
        let statistics = coordinator.get_agent_statistics(&agent_session_id);
        let root = ancestors
            .last()
            .map_or_else(|| statistics.clone(), |(_, statistics)| statistics.clone());
        Ok(Self {
            history,
            agent_session_id,
            parent_agent_session_id,
            broker,
            coordinator,
            statistics,
            root,
            ancestors: ancestors
                .into_iter()
                .map(|(_, statistics)| statistics)
                .collect(),
        })
    }

    pub fn capture(&self) -> AgentSessionStatisticsSnapshot {
        self.statistics.capture()
    }

    pub fn usage_snapshot(&self) -> SessionUsageSnapshot {
        self.coordinator.capture_usage(&self.root)
    }

    pub async fn record_request(
        &self,
        selected: &ProviderModel,
        reasoning: Option<&Reasoning>,
        completed: &Completion,
    ) -> Result<(), EventError> {
        let model = &selected.model;
        let increment = AgentUsageIncrement::from_completion(
            AgentUsageKey::from_request(
                selected.provider.id(),
                &model.id,
                reasoning.map(|reasoning| reasoning.effort.clone()),
            ),
            AgentCompletionUsage {
                input_tokens: i64::from(completed.input_tokens),
                cached_input_tokens: i64::from(completed.cached_input_tokens),
                output_tokens: i64::from(completed.output_tokens),
            },
            AgentUsagePricing {
                input_price: model.input_price,
                cached_input_price: model
                    .fields
                    .contains(ModelMetadataFields::CACHED_INPUT_PRICE)
                    .then_some(model.cached_input_price),
                output_price: model.output_price,
                context_window: i64::from(model.context_window),
            },
        );
        let request = RequestUsageRecorded {
            request_id: Identifier::event_id()?,
            parent_agent_session_id: self.parent_agent_session_id.clone(),
            provider: increment.key.provider.clone(),
            model: increment.key.model.clone(),
            effort: increment.key.effort.clone(),
            input_tokens: increment.totals.input_tokens,
            cached_input_tokens: increment.totals.cached_input_tokens,
            output_tokens: increment.totals.output_tokens,
            input_cost: increment.totals.input_cost,
            output_cost: increment.totals.output_cost,
            context_size: increment.context_size.unwrap_or_default(),
            context_limit: increment.context_limit.unwrap_or_default(),
            tool_call_ids: completed
                .tool_calls
                .iter()
                .map(|call| call.id.clone())
                .collect(),
            ..RequestUsageRecorded::new()
        };
        let mut fact = Event {
            id: Identifier::event_id()?,
            agent_session_id: self.agent_session_id.clone(),
            ..Event::new()
        };
        fact.set_request_usage_recorded(request);
        self.record(fact, increment).await
    }

    pub async fn record_tool(
        &self,
        assistant_sequence: i64,
        call: &ToolCall,
    ) -> Result<(), EventError> {
        let request = self.history.find_request_usage(call.id.clone()).await?;
        let key = request
            .as_ref()
            .map_or_else(AgentUsageKey::legacy, |request| {
                AgentUsageKey::from_request(
                    &request.provider,
                    &request.model,
                    request.effort.clone(),
                )
            });
        let execution = ToolExecutionStarted {
            request_id: request.map_or_else(String::new, |request| request.request_id),
            assistant_sequence,
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            parent_agent_session_id: self.parent_agent_session_id.clone(),
            provider: key.provider.clone(),
            model: key.model.clone(),
            effort: key.effort.clone(),
            ..ToolExecutionStarted::new()
        };
        let mut fact = Event {
            id: format!("tool-usage:{}:{}", self.agent_session_id, call.id),
            agent_session_id: self.agent_session_id.clone(),
            ..Event::new()
        };
        fact.set_tool_execution_started(execution);
        self.record(fact, AgentUsageIncrement::from_tool_start(key))
            .await
    }

    async fn record(&self, fact: Event, increment: AgentUsageIncrement) -> Result<(), EventError> {
        let statistics = self.statistics.clone();
        let ancestors = self.ancestors.clone();
        let broker = self.broker.clone();
        self.history
            .repository()
            .events()
            .record_usage(
                fact,
                increment,
                self.statistics.clone(),
                self.root.clone(),
                move |increment| {
                    statistics
                        .add_own(increment)
                        .expect("usage integer arithmetic overflow");
                    for ancestor in ancestors {
                        ancestor
                            .add_descendant(increment)
                            .expect("usage integer arithmetic overflow");
                    }
                },
                move |event| broker.publish_retained(&event),
            )
            .await
            .map(|_| ())
            .map_err(|error| match error.downcast::<EventError>() {
                Ok(error) => *error,
                Err(error) => EventError::Invalid(error.to_string()),
            })
    }
}
