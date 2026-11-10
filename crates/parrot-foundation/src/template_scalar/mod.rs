//! Source-compatible scalar prompt rendering through the shared core implementation.

use crate::config::{ConfigError, Result};
use parrot_core::{CancellationToken, Error, ErrorKind};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarPromptTemplateEngine(parrot_core::template::ScalarPromptTemplateEngine);

impl ScalarPromptTemplateEngine {
    pub fn compile(path: &str, text: &str, allowed: &BTreeSet<String>) -> Result<Self> {
        parrot_core::template::ScalarPromptTemplateEngine::compile(path, text, allowed)
            .map(Self)
            .map_err(|error| ConfigError::invalid_data(error.message))
    }

    pub fn render(
        &self,
        arguments: &BTreeMap<String, String>,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        let source = NativeCancellation(cancellation);
        self.0
            .render(arguments, CancellationToken::from_provider(&source))
            .map_err(translate_error)
    }
}

pub fn validate_arguments(
    path: &str,
    allowed: &BTreeSet<String>,
    required: &[String],
    names: &[String],
) -> Result<()> {
    parrot_core::template::validate_arguments(path, allowed, required, names)
        .map_err(|error| ConfigError::invalid_data(error.message))
}

pub(crate) fn translate_error(error: Error) -> OperationError<ConfigError> {
    if error.kind == ErrorKind::Cancelled {
        OperationError::Cancelled
    } else {
        OperationError::Failed(ConfigError::invalid_data(error.message))
    }
}

pub(crate) struct NativeCancellation<'a>(pub(crate) &'a dyn Cancellation);

impl parrot_core::Provider for NativeCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        self.0.cancelled()
    }
}
