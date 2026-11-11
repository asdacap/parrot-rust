use super::context::ContextReminderCheckpoint;
use super::message::{Content, Message, Role};
use super::{BoxFuture, CancellationToken, Error};

/// A prepared context reminder with the information needed to record it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminder {
    pub checkpoint: ContextReminderCheckpoint,
    pub content: String,
    pub observed_percentage: i32,
}

impl ContextReminder {
    pub fn render_message(&self) -> Message {
        Message {
            role: Role::System,
            contents: vec![Content::Text(self.content.clone())],
            ..Message::default()
        }
    }
}

/// Publishes core-selected notifications independently of conversation state.
pub trait EventPublisher: Send + Sync {
    /// Records and publishes the supplied event without adding conversation content.
    fn publish<'a>(
        &'a self,
        _event: crate::ConversationEvent,
        _cancellation: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("EventPublisher.publish")) })
    }
    /// Begins a prompt epoch before instructions are read, awaiting native refresh hooks.
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction start without request cancellation.
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes compaction failure without request cancellation.
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), Error>>;
    /// Publishes aggregate compaction progress without disclosing generated content.
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: super::compaction::CompactionProgress,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a;
}
