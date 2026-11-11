//! Shared concrete prompt compilation and bounded rendering.

mod minijinja;
mod scalar;

#[cfg(test)]
mod tests;

pub use minijinja::MiniJinjaPromptTemplateEngine;
pub use scalar::{ScalarPromptTemplateEngine, validate_arguments};

use crate::{CancellationToken, Error, PromptTemplate};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub(crate) enum Engine {
    Scalar(ScalarPromptTemplateEngine),
    MiniJinja(Box<MiniJinjaPromptTemplateEngine>),
}

impl PromptTemplate {
    pub fn compile(
        path: &str,
        text: &str,
        engine: &str,
        allowed: &[String],
        required: &[String],
    ) -> Result<Self, Error> {
        let allowed: BTreeSet<_> = allowed.iter().cloned().collect();
        let engine = match engine {
            "scalar" => Engine::Scalar(ScalarPromptTemplateEngine::compile(path, text, &allowed)?),
            "minijinja" => Engine::MiniJinja(Box::new(MiniJinjaPromptTemplateEngine::compile(
                path, text,
            )?)),
            _ => {
                return Err(Error::template(format!(
                    "{path}.engine must be scalar or minijinja"
                )));
            }
        };
        Ok(Self {
            path: path.to_owned(),
            allowed,
            required: required.to_vec(),
            engine,
        })
    }

    pub fn render(
        &self,
        arguments: &[(String, Value)],
        cancellation: CancellationToken<'_>,
    ) -> Result<String, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let names = arguments
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        validate_arguments(&self.path, &self.allowed, &self.required, &names)?;
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        match &self.engine {
            Engine::Scalar(engine) => {
                let values = arguments
                    .iter()
                    .filter_map(|(name, value)| {
                        value.as_str().map(|value| (name.clone(), value.to_owned()))
                    })
                    .collect();
                engine.render(&values, cancellation)
            }
            Engine::MiniJinja(engine) => {
                let values: Map<String, Value> = arguments.iter().cloned().collect();
                engine.render(&values, cancellation)
            }
        }
    }
}
