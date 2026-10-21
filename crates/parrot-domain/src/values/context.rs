use std::fmt;
use std::str::FromStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSize {
    value: i64,
    is_percentage: bool,
}

impl ContextSize {
    pub fn parse(text: &str) -> Result<Self, ParseContextSizeError> {
        let text = text.trim();
        let is_percentage = text.ends_with('%');
        let thousands = text.ends_with(['k', 'K']);
        let digits = if is_percentage || thousands {
            &text[..text.len() - 1]
        } else {
            text
        };
        if digits.is_empty() || !digits.bytes().all(|digit| digit.is_ascii_digit()) {
            return Err(ParseContextSizeError);
        }
        let parsed: i64 = digits.parse().map_err(|_| ParseContextSizeError)?;
        if parsed <= 0 || (is_percentage && parsed > 100) || (thousands && parsed > i64::MAX / 1000)
        {
            return Err(ParseContextSizeError);
        }
        Ok(Self {
            value: if thousands { parsed * 1000 } else { parsed },
            is_percentage,
        })
    }

    pub fn value(&self) -> i64 {
        self.value
    }

    pub fn is_percentage(&self) -> bool {
        self.is_percentage
    }

    pub fn resolve_tokens(&self, context_window: i32) -> Option<i64> {
        if self.is_percentage {
            (context_window > 0).then(|| i64::from(context_window) * self.value / 100)
        } else {
            Some(self.value)
        }
    }
}

impl FromStr for ContextSize {
    type Err = ParseContextSizeError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for ContextSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{}",
            self.value,
            if self.is_percentage { "%" } else { "" }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseContextSizeError;

impl fmt::Display for ParseContextSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Context size must be positive whole tokens, an integer k suffix, or a whole percentage from 1% to 100%.")
    }
}

impl std::error::Error for ParseContextSizeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionPolicy {
    pub trigger_tokens: Option<i64>,
    pub target_tokens: Option<i64>,
}

impl ContextCompactionPolicy {
    pub fn resolve(
        context_limit: Option<&ContextSize>,
        target_context_size: Option<&ContextSize>,
        context_window: i32,
        input_limit: i32,
        trigger_percent: i32,
        target_percent: i32,
        output_reserved_input_limit: Option<i64>,
    ) -> Self {
        let trigger_tokens = match context_limit {
            Some(limit) => cap_tokens(limit.resolve_tokens(context_window), input_limit),
            None => {
                (input_limit > 0).then(|| i64::from(input_limit) * i64::from(trigger_percent) / 100)
            }
        };
        let target_tokens = match target_context_size {
            Some(target) => cap_tokens(target.resolve_tokens(context_window), input_limit),
            None if context_limit.is_none() => {
                (input_limit > 0).then(|| i64::from(input_limit) * i64::from(target_percent) / 100)
            }
            None => trigger_tokens.map(|trigger| {
                (trigger / 100)
                    .wrapping_mul(i64::from(target_percent))
                    .wrapping_add((trigger % 100) * i64::from(target_percent) / 100)
            }),
        };
        let trigger_tokens = match (trigger_tokens, output_reserved_input_limit) {
            (Some(trigger), Some(safe_input_limit)) => Some(trigger.min(safe_input_limit)),
            (trigger, _) => trigger,
        };
        Self {
            trigger_tokens,
            target_tokens,
        }
    }
}

fn cap_tokens(tokens: Option<i64>, input_limit: i32) -> Option<i64> {
    if input_limit > 0 {
        tokens.map(|tokens| tokens.min(i64::from(input_limit)))
    } else {
        tokens
    }
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
    pub fn new(
        estimated_tokens: i64,
        context_limit: i32,
        usage_percent: Option<i32>,
        trigger_percent: i32,
    ) -> Self {
        Self {
            estimated_tokens,
            context_limit,
            usage_percent,
            trigger_percent,
            input_limit: context_limit,
            trigger_tokens: (context_limit > 0)
                .then(|| i64::from(context_limit) * i64::from(trigger_percent) / 100),
            has_context_limit_override: false,
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
            .is_some_and(|trigger_tokens| input_tokens > trigger_tokens)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminderCheckpoint {
    pub canonical_model: String,
    pub context_limit: i32,
    pub percentage: i32,
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

#[derive(Default)]
pub struct ContextCadence {
    canonical_model: Option<String>,
    context_window: Option<i32>,
    effective_history_size: i64,
    observed_band: i32,
    reported_band: i32,
    suppress_next_increase: bool,
    initialized: bool,
    checkpoint: Option<ContextReminderCheckpoint>,
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
    }

    pub fn observe(
        &mut self,
        snapshot: &ContextSnapshot,
        canonical_model: &str,
        effective_history_size: i64,
    ) -> Result<Option<i32>, ContextCadenceError> {
        validate_observation(canonical_model, effective_history_size)?;
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
        validate_observation(canonical_model, effective_history_size)?;
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

pub struct TokenEstimator;

impl TokenEstimator {
    pub fn estimate_tokens(text: &str) -> i64 {
        (text.encode_utf16().count() as i64 + 3) / 4
    }
}
