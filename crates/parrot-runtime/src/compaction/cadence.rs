use super::ContextSnapshot;
use parrot_core::{
    ContextCadence as CoreContextCadence, ContextCadenceError as CoreContextCadenceError,
    ContextReminderCheckpoint as CoreContextReminderCheckpoint,
    ContextSnapshot as CoreContextSnapshot,
};
use parrot_domain::values::context::{ContextCadenceError, ContextReminderCheckpoint};

#[derive(Default)]
pub struct ContextCadence {
    state: CoreContextCadence,
}

impl ContextCadence {
    pub const NOTIFICATION_INTERVAL: i32 = CoreContextCadence::NOTIFICATION_INTERVAL;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn band(snapshot: &ContextSnapshot) -> Option<i32> {
        CoreContextCadence::band(&CoreContextSnapshot {
            estimated_tokens: snapshot.estimated_tokens,
            context_limit: snapshot.context_limit,
            usage_percent: snapshot.usage_percent,
            trigger_percent: snapshot.trigger_percent,
            input_limit: snapshot.input_limit,
            trigger_tokens: snapshot.trigger_tokens,
            has_context_limit_override: snapshot.has_context_limit_override,
        })
    }

    pub fn restore(&mut self, checkpoint: Option<ContextReminderCheckpoint>) {
        self.state
            .restore(checkpoint.map(|value| CoreContextReminderCheckpoint {
                canonical_model: value.canonical_model,
                context_limit: value.context_limit,
                percentage: value.percentage,
            }));
    }

    pub fn observe(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<Option<i32>, ContextCadenceError> {
        self.state
            .observe(
                &CoreContextSnapshot {
                    estimated_tokens: snapshot.estimated_tokens,
                    context_limit: snapshot.context_limit,
                    usage_percent: snapshot.usage_percent,
                    trigger_percent: snapshot.trigger_percent,
                    input_limit: snapshot.input_limit,
                    trigger_tokens: snapshot.trigger_tokens,
                    has_context_limit_override: snapshot.has_context_limit_override,
                },
                canonical_model,
                effective_history_size,
            )
            .map_err(Self::translate_error)
    }

    pub fn acknowledge(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<(), ContextCadenceError> {
        self.state
            .acknowledge(
                &CoreContextSnapshot {
                    estimated_tokens: snapshot.estimated_tokens,
                    context_limit: snapshot.context_limit,
                    usage_percent: snapshot.usage_percent,
                    trigger_percent: snapshot.trigger_percent,
                    input_limit: snapshot.input_limit,
                    trigger_tokens: snapshot.trigger_tokens,
                    has_context_limit_override: snapshot.has_context_limit_override,
                },
                canonical_model,
                effective_history_size,
            )
            .map_err(Self::translate_error)
    }

    pub(crate) fn is_restored(&self) -> bool {
        self.state.is_restored()
    }

    pub(crate) fn capture_state(&self) -> CoreContextCadence {
        self.state.clone()
    }

    pub(crate) fn apply_state(&mut self, state: CoreContextCadence) {
        self.state = state;
    }

    fn translate_error(error: CoreContextCadenceError) -> ContextCadenceError {
        match error {
            CoreContextCadenceError::EmptyCanonicalModel => {
                ContextCadenceError::EmptyCanonicalModel
            }
            CoreContextCadenceError::NegativeEffectiveHistorySize => {
                ContextCadenceError::NegativeEffectiveHistorySize
            }
        }
    }
}
