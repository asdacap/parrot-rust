use super::support::*;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        storage::Committed,
    },
    protocol::messages::parrot::Event,
};
use parrot_runtime::event_broker::{BrokerError, EventBroker};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
pub struct AdmissionPublisher {
    pub broker: Arc<EventBroker>,
    pub gate: Mutex<Option<Arc<Gate>>>,
    pub reject: AtomicBool,
}
impl EventPublisher for AdmissionPublisher {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            if event.value().has_input_admitted() {
                let gate = self.gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.wait(cancellation)
                        .await
                        .map_err(|_| OperationError::Cancelled)?;
                }
                if self.reject.load(Ordering::SeqCst) {
                    return Err(OperationError::Failed(BrokerError::InvalidCapacity));
                }
            }
            self.broker.publish_committed(event, cancellation).await
        })
    }
    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        self.broker.publish_transient(event, cancellation)
    }
}
