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
