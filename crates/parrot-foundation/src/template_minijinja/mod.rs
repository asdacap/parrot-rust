//! Bounded native-Jinja prompt rendering under the A008 migration contract.
//!
//! This is not a Scriban compatibility layer. Inputs are owned JSON values only;
//! the private environment has native builtins but no loader or ambient callbacks.
//! Fuel bounds instructions, not time or memory. Without output, cancellation can
//! only be observed after native evaluation completes or exhausts its fuel.
use crate::config::{ConfigError, Result};
use minijinja::{AutoEscape, Environment, UndefinedBehavior, syntax::SyntaxConfig, value::Serde};
use parrot_domain::contracts::async_ops::{Cancellation, OperationError, OperationResult};
use serde_json::{Map, Value};
use std::io::{self, Write};

const FUEL_PER_RENDER: u64 = 1_000_000;
const RECURSION_LIMIT: usize = 500;

#[cfg(test)]
mod tests;

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
pub struct MiniJinjaPromptTemplateEngine {
    name: String,
    environment: Environment<'static>,
}

impl MiniJinjaPromptTemplateEngine {
    /// Compiles native Jinja synchronously, with path-prefixed native diagnostics.
    pub fn compile(path: &str, text: &str) -> Result<Self> {
        let name = format!("{path}.template");
        let mut environment = Environment::new();
        environment.set_undefined_behavior(UndefinedBehavior::Strict);
        environment.set_auto_escape_callback(|_| AutoEscape::None);
        environment.set_debug(false);
        environment.set_fuel(Some(FUEL_PER_RENDER));
        environment.set_recursion_limit(RECURSION_LIMIT);
        let syntax = SyntaxConfig::builder()
            .trim_blocks(false)
            .lstrip_blocks(false)
            .keep_trailing_newline(true)
            .build()
            .map_err(|error| ConfigError::invalid_data(format!("{name}: {error}")))?;
        environment.set_syntax(syntax);
        environment
            .add_template_owned(name.clone(), text.to_owned())
            .map_err(|error| ConfigError::invalid_data(format!("{name}: {error}")))?;
        Ok(Self { name, environment })
    }

    /// Renders safe JSON without leaking partial output on cancellation or failure.
    ///
    /// Cancellation is checked before conversion, before engine work, on output
    /// writes and after the engine returns, including native failure. Observed
    /// cancellation takes precedence over the native error.
    pub fn render(
        &self,
        arguments: &Map<String, Value>,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let context: minijinja::value::Value = Serde(arguments).into();
        let mut writer = CancellationWriter {
            cancellation,
            output: Vec::new(),
            cancelled: false,
        };
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let rendered = self
            .environment
            .get_template(&self.name)
            .and_then(|template| {
                template
                    .render_captured_to(context, &mut writer)
                    .map(|_| ())
            });
        let cancelled = cancellation.is_cancelled();
        if writer.cancelled || cancelled {
            return Err(OperationError::Cancelled);
        }
        rendered.map_err(|error| {
            OperationError::Failed(ConfigError::invalid_data(format!("{}: {error}", self.name)))
        })?;
        String::from_utf8(writer.output).map_err(|error| {
            OperationError::Failed(ConfigError::invalid_data(format!("{}: {error}", self.name)))
        })
    }
}

struct CancellationWriter<'a> {
    cancellation: &'a dyn Cancellation,
    output: Vec<u8>,
    cancelled: bool,
}

impl Write for CancellationWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            self.cancelled = true;
            // Interrupted is retried by write_all, so cancellation must not use it.
            return Err(io::Error::other("template rendering cancelled"));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
