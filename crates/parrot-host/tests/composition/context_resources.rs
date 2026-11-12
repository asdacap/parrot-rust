use super::*;
use parrot_runtime::agent_turn::usage::SessionUsageRecorder;
use parrot_storage::conversation::{AgentConversation, ConversationOrigin};

pub(super) struct BorrowedContextResources {
    pub snapshot: ToolSnapshot<Security>,
    pub selection: TurnSelection<Security>,
    pub conversation: AgentConversation,
    pub agent_state: parrot_core::AgentState,
    pub context: ContextController,
    pub prompt: SessionPrompt,
    pub sessions: sessions::ProviderSessions,
    pub usage: SessionUsageRecorder,
    pub events: ContextPublisher,
}

impl BorrowedContextResources {
    pub async fn new(fixture: &Fixture, handles: Vec<ToolHandle<Security>>) -> Self {
        Self {
            snapshot: ToolSnapshot::document(inventory(handles)).unwrap(),
            selection: selection(&Scripted::default()),
            conversation: AgentConversation::new(fixture.history.clone())
                .await
                .unwrap(),
            agent_state: parrot_core::AgentState::new(false),
            context: fixture.controller(),
            prompt: SessionPrompt::new(Box::new(Prompt)),
            sessions: sessions::ProviderSessions::new(),
            usage: SessionUsageRecorder::new(
                fixture.history.clone(),
                String::new(),
                Arc::new(EventBroker::new()),
                vec![],
            )
            .await
            .unwrap(),
            events: ContextPublisher {
                history: fixture.history.clone(),
                broker: EventBroker::new(),
            },
        }
    }
}

pub(super) struct ContextPublisher {
    history: AgentHistoryView,
    broker: EventBroker,
}

impl CompactionObserver for ContextPublisher {
    fn observe<'a>(
        &'a self,
        event: &'a sessions::Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| Error::Cancelled)?;
            if let sessions::Event::Retry {
                attempt, reason, ..
            } = event.base_event()
            {
                let mut event = Event {
                    id: id(),
                    agent_session_id: self.history.agent_session_id().into(),
                    ..Event::new()
                };
                event.set_retry_notice(protocol::RetryNotice {
                    reason: reason.clone(),
                    attempt: *attempt,
                    ..Default::default()
                });
                self.emit(event, cancellation)
                    .await
                    .map_err(context_error)?;
            }
            Ok(())
        })
    }

    fn observe_progress<'a>(
        &'a self,
        progress: protocol::CompactionProgressEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut event = Event {
                id: id(),
                agent_session_id: self.history.agent_session_id().into(),
                ..Event::new()
            };
            event.set_compaction_progress(progress);
            self.emit(event, cancellation).await.map_err(context_error)
        })
    }
}

impl ContextEvents for ContextPublisher {
    fn emit<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            self.history
                .append_message(event.clone(), None, ConversationOrigin::Model)
                .await?;
            self.publish_committed(Committed::after_commit(event), cancellation)
                .await
        })
    }

    fn publish_committed<'a>(
        &'a self,
        event: Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            self.broker
                .publish_committed(&event, cancellation)
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => ContextError::Cancelled,
                    OperationError::Failed(error) => ContextError::Operation(error.to_string()),
                })
        })
    }

    fn publish<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<()>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| ContextError::Cancelled)?;
            self.broker.publish_retained(&event);
            Ok(())
        })
    }
}

fn context_error(error: ContextError) -> Error {
    if matches!(error, ContextError::Cancelled) {
        Error::Cancelled
    } else {
        Error::Provider(error.to_string())
    }
}
