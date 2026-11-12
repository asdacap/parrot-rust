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

pub use parrot_core::ContextSnapshot;

pub struct TokenEstimator;

impl TokenEstimator {
    pub fn estimate_tokens(text: &str) -> i64 {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len() as i64
    }
}
