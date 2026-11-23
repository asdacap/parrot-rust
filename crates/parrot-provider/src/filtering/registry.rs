use super::{
    anthropic::AnthropicReasoningFilter, chat_completions::ChatCompletionsReasoningFilter,
    responses::ResponsesReasoningFilter,
};
use crate::routing::factory::ProviderImplementation;
use parrot_core::{Message, ReasoningFilter, ReasoningKind};
use parrot_foundation::config::models::ProviderConfig;
use std::{collections::BTreeMap, sync::Arc};

/// Builds the reasoning filter for every configured provider id. The provider
/// implementation decides the wire protocol and whether its reasoning must be
/// replayed intact.
pub fn reasoning_filters(
    providers: &BTreeMap<String, ProviderConfig>,
) -> BTreeMap<String, Arc<dyn ReasoningFilter>> {
    providers
        .iter()
        .map(|(id, config)| (id.clone(), resolve_filter(id, config)))
        .collect()
}

fn resolve_filter(id: &str, config: &ProviderConfig) -> Arc<dyn ReasoningFilter> {
    match ProviderImplementation::resolve(id, config) {
        ProviderImplementation::OpenAi | ProviderImplementation::ChatGpt => {
            Arc::new(ProtectedReasoningFilter)
        }
        ProviderImplementation::Anthropic => Arc::new(AnthropicReasoningFilter),
        _ => match config.protocol.as_str() {
            "responses" => Arc::new(ResponsesReasoningFilter),
            _ => Arc::new(ChatCompletionsReasoningFilter),
        },
    }
}

/// OpenAI and ChatGPT history keeps every reasoning representation intact, so
/// any message of theirs protects its batch.
struct ProtectedReasoningFilter;

impl ReasoningFilter for ProtectedReasoningFilter {
    fn kind(&self, _message: &Message) -> ReasoningKind {
        ReasoningKind::Protected
    }

    fn strip(&self, _message: &Message) -> Option<Message> {
        None
    }
}
