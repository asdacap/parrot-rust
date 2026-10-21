//! Offline decoding and merging for provider model catalogues.
//!
//! The field mask records whether a provider explicitly supplied a value. This
//! distinguishes an explicit zero/false/empty collection from absent metadata.

mod chatgpt;
mod json_read;
mod kimi;
mod litellm;
mod merge;
mod metadata;
mod models_dev;
mod openrouter;
mod standard;

pub use chatgpt::filter_chatgpt_external;
pub use kimi::KimiModelDecoder;
pub use litellm::LiteLlmModelInfoDecoder;
pub use merge::{merge_models, supplement_models};
pub use models_dev::ModelsDevCatalogueDecoder;
pub use openrouter::OpenRouterModelDecoder;
pub use standard::StandardModelDecoder;

use std::fmt;

/// Fields supplied by an endpoint, configuration, preset, or external source.
///
/// Values in [`Model`] are always materialized, while this mask preserves their
/// provenance for source-compatible precedence and explicit zero/false values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelMetadataFields(u16);

impl ModelMetadataFields {
    pub const NONE: Self = Self(0);
    pub const NAME: Self = Self(1 << 0);
    pub const CONTEXT_WINDOW: Self = Self(1 << 1);
    pub const MAX_OUTPUT_TOKENS: Self = Self(1 << 2);
    pub const INPUT_PRICE: Self = Self(1 << 3);
    pub const CACHED_INPUT_PRICE: Self = Self(1 << 4);
    pub const OUTPUT_PRICE: Self = Self(1 << 5);
    pub const TOOLS: Self = Self(1 << 6);
    pub const REASONING: Self = Self(1 << 7);
    pub const OUTPUT: Self = Self(1 << 8);
    pub const VARIANTS: Self = Self(1 << 9);
    pub const MAX_INPUT_TOKENS: Self = Self(1 << 10);

    pub const SUPPLEMENTAL: Self = Self(
        Self::CONTEXT_WINDOW.0
            | Self::MAX_OUTPUT_TOKENS.0
            | Self::MAX_INPUT_TOKENS.0
            | Self::INPUT_PRICE.0
            | Self::CACHED_INPUT_PRICE.0
            | Self::OUTPUT_PRICE.0
            | Self::TOOLS.0
            | Self::REASONING.0
            | Self::OUTPUT.0
            | Self::VARIANTS.0,
    );

    #[must_use]
    pub const fn contains(self, field: Self) -> bool {
        self.0 & field.0 == field.0
    }

    pub fn insert(&mut self, field: Self) {
        self.0 |= field.0;
    }

    pub fn remove(&mut self, field: Self) {
        self.0 &= !field.0;
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelVariant {
    pub name: String,
    pub reasoning_effort: String,
}

impl ModelVariant {
    #[must_use]
    pub fn new(name: impl Into<String>, reasoning_effort: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reasoning_effort: reasoning_effort.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCapabilities {
    pub tools: bool,
    pub reasoning: bool,
    pub output: Vec<String>,
    pub variants: Vec<ModelVariant>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub context_window: i32,
    pub max_output_tokens: i32,
    pub max_input_tokens: i32,
    pub input_price: f64,
    pub cached_input_price: f64,
    pub output_price: f64,
    pub capabilities: ModelCapabilities,
    pub fields: ModelMetadataFields,
}

impl Model {
    pub const DEFAULT_REQUESTED_OUTPUT_TOKENS: i32 = 32 * 1024;

    #[must_use]
    pub fn new(id: impl Into<String>, provider_id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: String::new(),
            id,
            provider_id: provider_id.into(),
            context_window: 0,
            max_output_tokens: 0,
            max_input_tokens: 0,
            input_price: 0.0,
            cached_input_price: 0.0,
            output_price: 0.0,
            capabilities: ModelCapabilities::default(),
            fields: ModelMetadataFields::NONE,
        }
    }

    #[must_use]
    pub fn requested_output_tokens(&self) -> i32 {
        if self.max_output_tokens > 0 {
            Self::DEFAULT_REQUESTED_OUTPUT_TOKENS.min(self.max_output_tokens)
        } else {
            Self::DEFAULT_REQUESTED_OUTPUT_TOKENS
        }
    }

    #[must_use]
    pub fn output_reserved_input_token_limit(&self) -> Option<i64> {
        (self.context_window > 0).then(|| {
            ((i64::from(self.context_window) - i64::from(self.requested_output_tokens())) * 20 / 21)
                .max(0)
        })
    }

    #[must_use]
    pub fn input_token_limit(&self) -> i32 {
        if self.context_window > 0 && self.max_input_tokens > 0 {
            self.context_window.min(self.max_input_tokens)
        } else {
            self.context_window.max(self.max_input_tokens)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogError(pub String);

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CatalogError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StandardModelError {
    Json(CatalogError),
    InvalidOperation(CatalogError),
    Provider(CatalogError),
}

impl StandardModelError {
    pub(crate) fn into_catalog_error(self) -> CatalogError {
        match self {
            Self::Json(error) | Self::InvalidOperation(error) | Self::Provider(error) => error,
        }
    }
}

impl fmt::Display for StandardModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) | Self::InvalidOperation(error) | Self::Provider(error) => {
                fmt::Display::fmt(error, formatter)
            }
        }
    }
}

impl std::error::Error for StandardModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) | Self::InvalidOperation(error) | Self::Provider(error) => {
                Some(error)
            }
        }
    }
}

pub(crate) fn supported_effort(value: &str) -> bool {
    matches!(
        value,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

pub(crate) fn from_efforts(efforts: &[String], default: &str) -> Vec<ModelVariant> {
    let mut ordered = efforts.to_vec();
    if let Some(index) = ordered.iter().position(|effort| effort == default) {
        if !default.is_empty() && index > 0 {
            ordered.swap(0, index);
        }
    }
    ordered
        .into_iter()
        .map(|effort| ModelVariant::new(&effort, &effort))
        .collect()
}
