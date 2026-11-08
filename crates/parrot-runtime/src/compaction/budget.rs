use super::PlanningError;
use parrot_core::{ContextModel, OutputTokenError, TokenCalibration};
use parrot_provider::{catalogs::Model, sessions::Completion};

/// Source ProviderTokenBudget calibration belongs to one canonical selector.
#[derive(Debug, Default)]
pub struct ProviderTokenBudget {
    calibration: TokenCalibration,
}

impl ProviderTokenBudget {
    pub const DEFAULT_MAXIMUM_OUTPUT_TOKENS: i32 = ContextModel::DEFAULT_REQUESTED_OUTPUT_TOKENS;

    pub fn estimate_input_tokens(&self, selector: &str, estimated_input_tokens: i64) -> i64 {
        self.calibration
            .estimate_input_tokens(selector, estimated_input_tokens)
    }

    pub fn observe_usage(&mut self, selector: &str, estimate: i64, completed: &Completion) {
        self.calibration
            .observe_usage(selector, estimate, completed.input_tokens);
    }

    pub fn reset(&mut self) {
        self.calibration.reset();
    }

    pub fn calculate_maximum_output_tokens(
        &self,
        selector: &str,
        estimate: i64,
        model: &Model,
    ) -> Result<i32, PlanningError> {
        self.calibration
            .calculate_maximum_output_tokens(
                &ContextModel {
                    canonical_model: selector.into(),
                    context_window: model.context_window,
                    maximum_input_tokens: model.max_input_tokens,
                    maximum_output_tokens: model.max_output_tokens,
                },
                estimate,
            )
            .map_err(|error| match error {
                OutputTokenError::InputLimitExceeded => PlanningError::InputLimitExceeded,
                OutputTokenError::NoOutputCapacity => PlanningError::NoOutputCapacity,
            })
    }

    pub(crate) fn capture_state(&self) -> TokenCalibration {
        self.calibration.clone()
    }
}
