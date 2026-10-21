use super::PlanningError;
use parrot_provider::{catalogs::Model, sessions::Completion};

/// Source ProviderTokenBudget calibration belongs to one canonical selector.
#[derive(Debug, Default)]
pub struct ProviderTokenBudget {
    selector: Option<String>,
    estimated_input_tokens: i64,
    actual_input_tokens: i32,
}

impl ProviderTokenBudget {
    pub const DEFAULT_MAXIMUM_OUTPUT_TOKENS: i32 = 32 * 1024;

    pub fn estimate_input_tokens(&self, selector: &str, estimated_input_tokens: i64) -> i64 {
        let estimate = estimated_input_tokens.max(0);
        if self.selector.as_deref() != Some(selector) {
            return estimate;
        }
        let calibration =
            (i64::from(self.actual_input_tokens) - self.estimated_input_tokens).max(0);
        estimate.saturating_add(calibration)
    }

    pub fn observe_usage(&mut self, selector: &str, estimate: i64, completed: &Completion) {
        if completed.input_tokens <= 0 {
            return;
        }
        self.selector = Some(selector.to_owned());
        self.estimated_input_tokens = estimate.max(0);
        self.actual_input_tokens = completed.input_tokens;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn calculate_maximum_output_tokens(
        &self,
        selector: &str,
        estimate: i64,
        model: &Model,
    ) -> Result<i32, PlanningError> {
        let input = self.estimate_input_tokens(selector, estimate);
        if model.max_input_tokens > 0 && input > i64::from(model.input_token_limit()) {
            return Err(PlanningError::InputLimitExceeded);
        }
        let mut output = if model.max_output_tokens > 0 {
            Self::DEFAULT_MAXIMUM_OUTPUT_TOKENS.min(model.max_output_tokens)
        } else {
            Self::DEFAULT_MAXIMUM_OUTPUT_TOKENS
        };
        if model.context_window > 0 {
            let headroom = input / 20 + i64::from(input % 20 > 0);
            let available = i64::from(model.context_window) - input;
            if available <= headroom {
                return Err(PlanningError::NoOutputCapacity);
            }
            output = i64::from(output).min(available - headroom) as i32;
        }
        Ok(output)
    }
}
