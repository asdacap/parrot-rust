//! Conversation history filtering without model invocation.

use parrot_core::{
    BoxFuture, CancellationToken, Error, FilterProvider, FilterRequest, Message, ReasoningFilter,
    ReasoningKind,
};
use parrot_foundation::config::models::ProviderConfig;
use parrot_provider::filtering::reasoning_filters;
use std::{collections::BTreeMap, sync::Arc};

/// Strips reasoning payloads from replayable provider content by asking each
/// message's provider filter to classify its own raw content. A batch carrying
/// protected reasoning, meaning reasoning the provider must replay intact,
/// stays untouched as a whole.
pub struct ReasoningSkipFilter {
    filters: BTreeMap<String, Arc<dyn ReasoningFilter>>,
}

impl ReasoningSkipFilter {
    pub fn new(providers: &BTreeMap<String, ProviderConfig>) -> Self {
        Self {
            filters: reasoning_filters(providers),
        }
    }
}

impl FilterProvider for ReasoningSkipFilter {
    fn filter<'a, 'c>(
        &'a self,
        request: FilterRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Vec<Message>>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let mut changed = false;
            let mut replacements = Vec::with_capacity(request.messages.len());
            for message in request.messages {
                let filter = self.filters.get(&message.provider_id);
                let kind = filter.map_or(ReasoningKind::Absent, |filter| filter.kind(&message));
                match kind {
                    ReasoningKind::Protected => return Ok(None),
                    ReasoningKind::Plain => {
                        match filter.and_then(|filter| filter.strip(&message)) {
                            Some(stripped) => {
                                changed = true;
                                replacements.push(stripped);
                            }
                            None => replacements.push(message),
                        }
                    }
                    ReasoningKind::Absent => replacements.push(message),
                }
            }
            Ok(changed.then_some(replacements))
        })
    }
}

#[cfg(test)]
mod tests;
