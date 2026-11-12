//! On-demand completion context observations, with completion policy owned by core.
use super::{blockers::ActiveWorkReminder, exit_reminders::ExitReminders};
use crate::{
    agent_turn::services::{CompletionStateSource, TurnError},
    event_broker::BrokerError,
    interactions::AgentAsks,
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        events::EventPublisher,
    },
    protocol::messages::parrot::Event,
};
use std::sync::Arc;

/// Supply the same reminder Arc to this source and `RuntimeConfig.reminders`.
/// Native state orders these observations; core retains its candidate for any blockers.
pub struct SessionCompletion<P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized> {
    pub asks: Option<Arc<AgentAsks>>,
    pub active_work: Arc<ActiveWorkReminder>,
    pub reminders: Arc<ExitReminders<P>>,
}
impl<P> CompletionStateSource for SessionCompletion<P>
where
    P: EventPublisher<Event = Event, Error = BrokerError> + ?Sized,
{
    fn get_pending_ask_reminder<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            match &self.asks {
                Some(asks) => asks
                    .completion_reminder()
                    .map_err(|error| TurnError::Service(error.to_string())),
                None => Ok(None),
            }
        })
    }
    fn get_active_work_reminder<'a>(
        &'a self,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(self.active_work.build())
    }
    fn get_exit_reminder<'a>(
        &'a self,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            self.reminders
                .build()
                .await
                .map_err(|error| TurnError::Service(error.to_string()))
        })
    }
}
