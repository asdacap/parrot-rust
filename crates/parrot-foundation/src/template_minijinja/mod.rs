//! Bounded native-Jinja prompt rendering through the shared core implementation.

use crate::config::{ConfigError, Result};
use crate::template_scalar::{NativeCancellation, translate_error};
use parrot_core::CancellationToken;
use parrot_domain::contracts::async_ops::{Cancellation, OperationResult};
use serde_json::{Map, Value};

/// An owned compiled template. Each call creates a fresh native evaluation state.
///
/// The API deliberately cannot accept host objects, functions or MiniJinja values.
///
/// ```compile_fail
/// use parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine;
/// use minijinja::value::Value;
/// use parrot_domain::contracts::async_ops::Cancellation;
/// fn host_values(engine: &MiniJinjaPromptTemplateEngine, cancellation: &dyn Cancellation) {
///     engine.render(&Value::from(42), cancellation);
/// }
/// ```
pub struct MiniJinjaPromptTemplateEngine(parrot_core::template::MiniJinjaPromptTemplateEngine);

impl MiniJinjaPromptTemplateEngine {
    pub fn compile(path: &str, text: &str) -> Result<Self> {
        parrot_core::template::MiniJinjaPromptTemplateEngine::compile(path, text)
            .map(Self)
            .map_err(|error| ConfigError::invalid_data(error.message))
    }

    pub fn render(
        &self,
        arguments: &Map<String, Value>,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        let source = NativeCancellation(cancellation);
        self.0
            .render(arguments, CancellationToken::from_provider(&source))
            .map_err(translate_error)
    }
}
