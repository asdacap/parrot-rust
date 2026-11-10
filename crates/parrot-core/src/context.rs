use crate::{
    ContextConfiguration, ContextLimit, ContextModel, ContextReminderCheckpoint, ContextSnapshot,
    TokenCalibration,
};
mod preparation;
pub(crate) use preparation::{compact_context, prepare_request};

use crate::{Error, ErrorCode, ErrorKind};
use std::fmt;

impl ContextLimit {
    pub fn resolve_tokens(&self, context_window: i32) -> Option<i64> {
        match self {
            Self::Tokens(tokens) => Some(*tokens),
            Self::Percentage(percentage) => {
                (context_window > 0).then(|| i64::from(context_window) * *percentage / 100)
            }
        }
    }
}

impl ContextModel {
    pub const DEFAULT_REQUESTED_OUTPUT_TOKENS: i32 = 32 * 1024;

    pub fn calculate_input_limit(&self) -> i32 {
        if self.context_window > 0 && self.maximum_input_tokens > 0 {
            self.context_window.min(self.maximum_input_tokens)
        } else {
            self.context_window.max(self.maximum_input_tokens)
        }
    }

    pub fn calculate_requested_output_tokens(&self) -> i32 {
        if self.maximum_output_tokens > 0 {
            Self::DEFAULT_REQUESTED_OUTPUT_TOKENS.min(self.maximum_output_tokens)
        } else {
            Self::DEFAULT_REQUESTED_OUTPUT_TOKENS
        }
    }

    pub fn calculate_output_reserved_input_limit(&self) -> Option<i64> {
        (self.context_window > 0).then(|| {
            ((i64::from(self.context_window) - i64::from(self.calculate_requested_output_tokens()))
                * 20
                / 21)
                .max(0)
        })
    }
}

impl ContextSnapshot {
    pub fn estimate(estimated_tokens: i64, configuration: &ContextConfiguration) -> Self {
        let context_limit = configuration.model.context_window;
        let input_limit = configuration.model.calculate_input_limit();
        let usage_percent = (context_limit > 0).then(|| {
            if estimated_tokens >= i64::from(context_limit) {
                100
            } else {
                ((i128::from(estimated_tokens) * 100 / i128::from(context_limit)).clamp(0, 100))
                    as i32
            }
        });
        let trigger_tokens = match &configuration.context_limit {
            Some(limit) => {
                let tokens = limit.resolve_tokens(context_limit);
                if input_limit > 0 {
                    tokens.map(|tokens| tokens.min(i64::from(input_limit)))
                } else {
                    tokens
                }
            }
            None => (input_limit > 0)
                .then(|| i64::from(input_limit) * i64::from(configuration.trigger_percent) / 100),
        };
        let trigger_tokens = match (
            trigger_tokens,
            configuration.model.calculate_output_reserved_input_limit(),
        ) {
            (Some(trigger), Some(safe_input_limit)) => Some(trigger.min(safe_input_limit)),
            (trigger, _) => trigger,
        };
        let mut trigger_percent = configuration.trigger_percent;
        if configuration.context_limit.is_some() && context_limit > 0 {
            if let Some(trigger) = trigger_tokens {
                trigger_percent = if trigger >= i64::from(context_limit) {
                    100
                } else {
                    ((i128::from(trigger) * 100 / i128::from(context_limit)).clamp(0, 100)) as i32
                };
            }
        }
        Self {
            estimated_tokens,
            context_limit,
            usage_percent,
            trigger_percent,
            input_limit,
            trigger_tokens,
            has_context_limit_override: configuration.context_limit.is_some(),
        }
    }

    pub fn select_reminder_percentage(
        &self,
        canonical_model: &str,
        last_reminder: Option<&ContextReminderCheckpoint>,
    ) -> Option<i32> {
        let interval = ContextReminderCheckpoint::NOTIFICATION_INTERVAL;
        let percentage = self.usage_percent? / interval * interval;
        if percentage < 50
            || last_reminder.is_some_and(|last| {
                last.canonical_model == canonical_model
                    && last.context_limit == self.context_limit
                    && last.percentage >= percentage
            })
        {
            return None;
        }
        Some(percentage)
    }

    pub fn is_available(&self) -> bool {
        self.context_limit > 0
    }

    pub fn exceeds_input_limit(&self) -> bool {
        self.input_limit > 0 && self.estimated_tokens > i64::from(self.input_limit)
    }

    pub fn exceeds_trigger(&self) -> bool {
        self.exceeds_compaction_trigger(self.estimated_tokens)
    }

    pub fn exceeds_compaction_trigger(&self, input_tokens: i64) -> bool {
        self.trigger_tokens
            .is_some_and(|trigger| input_tokens > trigger)
    }

    pub fn requires_compaction(&self, calibration: &TokenCalibration, selector: &str) -> bool {
        self.exceeds_trigger()
            || (self.input_limit > 0
                && self.exceeds_compaction_trigger(
                    calibration.estimate_input_tokens(selector, self.estimated_tokens),
                ))
    }

    pub fn ensure_input_fits(&self) -> Result<(), Error> {
        if self.exceeds_input_limit() {
            return Err(Error {
                kind: ErrorKind::Failed,
                code: ErrorCode::InvalidOperation,
                message: "The compacted conversation exceeds the selected model input limit."
                    .into(),
                response_body: String::new(),
                detail: None,
            });
        }
        Ok(())
    }
}

impl TokenCalibration {
    pub fn estimate_input_tokens(&self, selector: &str, estimated_input_tokens: i64) -> i64 {
        let estimate = estimated_input_tokens.max(0);
        if self.selector.as_deref() != Some(selector) {
            return estimate;
        }
        let calibration =
            (i64::from(self.actual_input_tokens) - self.estimated_input_tokens).max(0);
        estimate.saturating_add(calibration)
    }

    pub fn observe_usage(&mut self, selector: &str, estimate: i64, actual_input_tokens: i32) {
        if actual_input_tokens <= 0 {
            return;
        }
        self.selector = Some(selector.to_owned());
        self.estimated_input_tokens = estimate.max(0);
        self.actual_input_tokens = actual_input_tokens;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn calculate_maximum_output_tokens(
        &self,
        model: &ContextModel,
        estimate: i64,
    ) -> Result<i32, OutputTokenError> {
        let input = self.estimate_input_tokens(&model.canonical_model, estimate);
        if model.maximum_input_tokens > 0 && input > i64::from(model.calculate_input_limit()) {
            return Err(OutputTokenError::InputLimitExceeded);
        }
        let mut output = model.calculate_requested_output_tokens();
        if model.context_window > 0 {
            let headroom = input / 20 + i64::from(input % 20 > 0);
            let available = i64::from(model.context_window) - input;
            if available <= headroom {
                return Err(OutputTokenError::NoOutputCapacity);
            }
            output = i64::from(output).min(available - headroom) as i32;
        }
        Ok(output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputTokenError {
    InputLimitExceeded,
    NoOutputCapacity,
}

impl fmt::Display for OutputTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InputLimitExceeded => "The conversation exceeds the selected model input limit.",
            Self::NoOutputCapacity => {
                "The conversation leaves no output capacity in the selected model context window."
            }
        })
    }
}

impl std::error::Error for OutputTokenError {}

impl From<OutputTokenError> for Error {
    fn from(value: OutputTokenError) -> Self {
        Self {
            kind: ErrorKind::Failed,
            code: ErrorCode::InvalidOperation,
            message: value.to_string(),
            response_body: String::new(),
            detail: None,
        }
    }
}

impl ContextReminderCheckpoint {
    pub const NOTIFICATION_INTERVAL: i32 = 10;
}
