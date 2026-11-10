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
        parrot_core::ContextLimit::from(self).resolve_tokens(context_window)
    }
}

impl From<&ContextSize> for parrot_core::ContextLimit {
    fn from(size: &ContextSize) -> Self {
        if size.is_percentage {
            Self::Percentage(size.value)
        } else {
            Self::Tokens(size.value)
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
        let context_limit = context_limit.map(Into::into);
        let target_context_size = target_context_size.map(Into::into);
        let policy = parrot_core::compaction::CompactionPolicy::resolve(
            context_limit.as_ref(),
            target_context_size.as_ref(),
            context_window,
            input_limit,
            trigger_percent,
            target_percent,
            output_reserved_input_limit,
        );
        Self {
            trigger_tokens: policy.trigger_tokens,
            target_tokens: policy.target_tokens,
        }
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

pub struct TokenEstimator;

impl TokenEstimator {
    pub fn estimate_tokens(text: &str) -> i64 {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len() as i64
    }
}
