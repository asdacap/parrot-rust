use super::{BoxFuture, CancellationToken, Error, Message};

/// The filter configuration core reads through `ConfigKey::Filter`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilterSettings {
    pub enabled: bool,
    /// The unfiltered history tail triggers one filter batch once its estimated
    /// size has reached this token count.
    pub max_unfiltered_token_window: i64,
    /// The smallest batch the core submits, rounded up to the next conversation
    /// boundary.
    pub filter_batch_size: i64,
}

/// One filter batch the core submits from the unfiltered history window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilterRequest {
    pub messages: Vec<Message>,
}

/// The durable facts of one filter commit: the replaced history span, its
/// replacement messages, and the new filter watermark.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilterAmendment {
    /// First history sequence the replacement supersedes.
    pub start_sequence: i64,
    /// Last history sequence the replacement supersedes.
    pub end_sequence: i64,
    /// Replacement messages, exactly one per superseded row, in sequence order.
    /// A skipped batch resubmits its original messages and only moves the
    /// watermark.
    pub messages: Vec<Message>,
    /// The durable filter watermark after this commit. Every batch advances it,
    /// including a batch the filter skipped, so no batch is ever re-offered.
    pub watermark: i64,
    /// Estimated token count of the superseded span before filtering.
    pub from_tokens: i64,
    /// Estimated token count of the replacement messages after filtering.
    pub to_tokens: i64,
}

/// Filters one batch of conversation history and returns its replacement messages.
/// Returning `Ok(None)` marks the whole batch as a no-op, for example when the
/// batch carries encrypted reasoning the provider must replay intact.
pub trait FilterProvider: Send + Sync {
    fn filter<'a, 'c>(
        &'a self,
        _request: FilterRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Vec<Message>>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async { Err(Error::unsupported("FilterProvider.filter")) })
    }
}

/// The provider's classification of one message's raw provider content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningKind {
    /// No reasoning members, or nothing this provider can remove.
    Absent,
    /// Reasoning members the provider can strip from the message.
    Plain,
    /// Reasoning members that must be replayed intact and block the batch.
    Protected,
}

/// Provider-side reasoning classification and stripping for one protocol.
/// Implementations own their raw provider content format; the filter only
/// reads `kind` and delegates the rewrite to `strip`.
pub trait ReasoningFilter: Send + Sync {
    /// Classifies the reasoning members carried by this message.
    fn kind(&self, message: &Message) -> ReasoningKind;

    /// The message with its strippable reasoning removed, or `None` when the
    /// message is unchanged.
    fn strip(&self, message: &Message) -> Option<Message>;
}
