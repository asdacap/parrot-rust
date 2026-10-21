//! Source-compatible scalar prompt template compilation and rendering.
//!
//! Scalar insertion is deliberately opaque: values are appended exactly once and are never
//! parsed as another template. Scriban parsing/evaluation belongs to a different engine.
use crate::config::{ConfigError, Result};
use parrot_domain::contracts::async_ops::{Cancellation, OperationError, OperationResult};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Part {
    Literal(String),
    Argument(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarPromptTemplateEngine {
    path: String,
    parts: Vec<Part>,
}

impl ScalarPromptTemplateEngine {
    /// Compiles the scalar brace grammar against the descriptor's ordinal argument names.
    pub fn compile(path: &str, text: &str, allowed: &BTreeSet<String>) -> Result<Self> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut index = 0;
        while index < text.len() {
            let character = text[index..]
                .chars()
                .next()
                .expect("index is a char boundary");
            let next_index = index + character.len_utf8();
            if character == '{' && text[next_index..].starts_with('{') {
                literal.push('{');
                index = next_index + 1;
                continue;
            }
            if character == '}' && text[next_index..].starts_with('}') {
                literal.push('}');
                index = next_index + 1;
                continue;
            }
            if character == '}' {
                return Err(ConfigError::invalid_data(format!(
                    "{path}.template contains an unmatched closing brace"
                )));
            }
            if character != '{' {
                literal.push(character);
                index = next_index;
                continue;
            }

            let close = text[next_index..]
                .find('}')
                .map(|relative| next_index + relative)
                .ok_or_else(|| {
                    ConfigError::invalid_data(format!(
                        "{path}.template contains an unmatched opening brace"
                    ))
                })?;
            let placeholder = &text[next_index..close];
            if !allowed.contains(placeholder) {
                return Err(ConfigError::invalid_data(format!(
                    "{path}.template contains unknown placeholder '{placeholder}'"
                )));
            }
            if parts
                .iter()
                .any(|part| matches!(part, Part::Argument(name) if name == placeholder))
            {
                return Err(ConfigError::invalid_data(format!(
                    "{path}.template contains duplicate placeholder '{placeholder}'"
                )));
            }
            if !literal.is_empty() {
                parts.push(Part::Literal(std::mem::take(&mut literal)));
            }
            parts.push(Part::Argument(placeholder.to_owned()));
            index = close + 1;
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Ok(Self {
            path: path.to_owned(),
            parts,
        })
    }

    /// Renders only scalar values. Cancellation is observed at the same pre-render boundary as
    /// the source engine; cancellation remains distinct from a template error.
    pub fn render(
        &self,
        arguments: &BTreeMap<String, String>,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let mut output = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => output.push_str(text),
                Part::Argument(name) => match arguments.get(name) {
                    Some(value) => output.push_str(value),
                    None => {
                        return Err(OperationError::Failed(ConfigError::invalid_data(format!(
                            "{} requires scalar argument '{name}'",
                            self.path
                        ))));
                    }
                },
            }
        }
        Ok(output)
    }
}

/// Validates a catalog call without imposing a map's sorting order on source argument names.
/// `names` and `required` retain the exact supplied order for duplicate/diagnostic behavior.
pub fn validate_arguments(
    path: &str,
    allowed: &BTreeSet<String>,
    required: &[String],
    names: &[String],
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(ConfigError::invalid_data(format!(
                "{path} received duplicate argument '{name}'"
            )));
        }
    }
    for name in names {
        if !allowed.contains(name) {
            return Err(ConfigError::invalid_data(format!(
                "{path} does not allow argument '{name}'"
            )));
        }
    }
    for name in required {
        if !seen.contains(name.as_str()) {
            return Err(ConfigError::invalid_data(format!(
                "{path} requires argument '{name}'"
            )));
        }
    }
    Ok(())
}
