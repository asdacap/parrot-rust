mod preparation;
pub(crate) use preparation::prepare_request;

use crate::{Error, ErrorCode, ErrorKind};
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextLimit {
    Tokens(i64),
    Percentage(i64),
}

impl ContextLimit {
    fn resolve_tokens(&self, context_window: i32) -> Option<i64> {
        match self {
            Self::Tokens(tokens) => Some(*tokens),
            Self::Percentage(percentage) => {
                (context_window > 0).then(|| i64::from(context_window) * *percentage / 100)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextModel {
    pub canonical_model: String,
    pub context_window: i32,
    pub maximum_input_tokens: i32,
    pub maximum_output_tokens: i32,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextConfiguration {
    pub model: ContextModel,
    pub trigger_percent: i32,
    pub context_limit: Option<ContextLimit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextState {
    pub configuration: ContextConfiguration,
    pub calibration: TokenCalibration,
    pub cadence: ContextCadence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub estimated_tokens: i64,
    pub context_limit: i32,
    pub usage_percent: Option<i32>,
    pub trigger_percent: i32,
    pub input_limit: i32,
    pub trigger_tokens: Option<i64>,
    pub has_context_limit_override: bool,
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
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminderCheckpoint {
    pub canonical_model: String,
    pub context_limit: i32,
    pub percentage: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    pub instructions: String,
    pub reduced: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenCalibration {
    selector: Option<String>,
    estimated_input_tokens: i64,
    actual_input_tokens: i32,
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
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCadenceError {
    EmptyCanonicalModel,
    NegativeEffectiveHistorySize,
}

impl fmt::Display for ContextCadenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyCanonicalModel => "canonical model must not be empty or whitespace",
            Self::NegativeEffectiveHistorySize => "effective history size must not be negative",
        })
    }
}

impl std::error::Error for ContextCadenceError {}

impl From<ContextCadenceError> for Error {
    fn from(value: ContextCadenceError) -> Self {
        Self {
            kind: ErrorKind::Failed,
            code: ErrorCode::Unexpected,
            message: value.to_string(),
            response_body: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextCadence {
    canonical_model: Option<String>,
    context_window: Option<i32>,
    effective_history_size: i64,
    observed_band: i32,
    reported_band: i32,
    suppress_next_increase: bool,
    initialized: bool,
    checkpoint: Option<ContextReminderCheckpoint>,
    restored: bool,
}

impl ContextCadence {
    pub const NOTIFICATION_INTERVAL: i32 = 10;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn band(snapshot: &ContextSnapshot) -> Option<i32> {
        snapshot
            .usage_percent
            .filter(|percentage| *percentage > 0)
            .map(|percentage| percentage / Self::NOTIFICATION_INTERVAL)
    }

    pub fn restore(&mut self, checkpoint: Option<ContextReminderCheckpoint>) {
        self.checkpoint = checkpoint;
        self.restored = true;
    }

    pub fn is_restored(&self) -> bool {
        self.restored
    }

    pub fn observe(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<Option<i32>, ContextCadenceError> {
        Self::validate_observation(canonical_model, effective_history_size)?;
        let band = Self::band(snapshot).unwrap_or(0);
        if !self.initialized {
            let reported_band = self
                .checkpoint
                .as_ref()
                .filter(|checkpoint| {
                    checkpoint.canonical_model == canonical_model
                        && checkpoint.context_limit == snapshot.context_limit
                })
                .map_or(0, |checkpoint| {
                    checkpoint.percentage / Self::NOTIFICATION_INTERVAL
                });
            self.set_state(
                snapshot,
                canonical_model,
                effective_history_size,
                band,
                reported_band,
            );
            self.checkpoint = None;
            return Ok(None);
        }
        if self.canonical_model.as_deref() != Some(canonical_model)
            || self.context_window != Some(snapshot.context_limit)
        {
            self.set_state(snapshot, canonical_model, effective_history_size, band, 0);
            return Ok(None);
        }
        if effective_history_size < self.effective_history_size {
            self.set_state(
                snapshot,
                canonical_model,
                effective_history_size,
                band,
                band,
            );
            self.suppress_next_increase = true;
            return Ok(None);
        }
        self.effective_history_size = effective_history_size;
        if band <= self.observed_band {
            return Ok(None);
        }
        if self.suppress_next_increase {
            self.suppress_next_increase = false;
            self.observed_band = band;
            return Ok(None);
        }
        self.observed_band = band;
        if band * Self::NOTIFICATION_INTERVAL < 50 || band <= self.reported_band {
            return Ok(None);
        }
        self.reported_band = band;
        Ok(Some(band * Self::NOTIFICATION_INTERVAL))
    }

    pub fn acknowledge(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<(), ContextCadenceError> {
        Self::validate_observation(canonical_model, effective_history_size)?;
        let band = Self::band(snapshot).unwrap_or(0);
        self.set_state(
            snapshot,
            canonical_model,
            effective_history_size,
            band,
            band,
        );
        Ok(())
    }

    fn set_state(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
        observed_band: i32,
        reported_band: i32,
    ) {
        self.canonical_model = Some(canonical_model.to_owned());
        self.context_window = Some(snapshot.context_limit);
        self.effective_history_size = effective_history_size;
        self.observed_band = observed_band;
        self.reported_band = reported_band;
        self.suppress_next_increase = false;
        self.initialized = true;
    }

    fn validate_observation(
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<(), ContextCadenceError> {
        if canonical_model.trim().is_empty() {
            return Err(ContextCadenceError::EmptyCanonicalModel);
        }
        if effective_history_size < 0 {
            return Err(ContextCadenceError::NegativeEffectiveHistorySize);
        }
        Ok(())
    }
}
