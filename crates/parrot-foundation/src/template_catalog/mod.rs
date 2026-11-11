//! Scalar/native-Jinja catalog composition using the real configuration validator.
//!
//! Descriptor and argument order are retained. Inputs are owned JSON values, not
//! host objects or engine values; scalar insertion accepts actual strings only.
use crate::config::{ConfigError, Configuration, Result, templates::TemplateSpec};
use parrot_core::{CancellationToken, Error, ErrorKind, PromptTemplate};
use parrot_domain::contracts::async_ops::{Cancellation, OperationError, OperationResult};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
mod tests;

struct Template {
    id: String,
    compiled: Arc<PromptTemplate>,
}

impl Template {
    fn compile(spec: &TemplateSpec) -> Result<Self> {
        let id = spec.path.strip_prefix("prompt_templates.").ok_or_else(|| {
            ConfigError::invalid_data(format!("{} must be a prompt template path", spec.path))
        })?;
        let compiled = PromptTemplate::compile(
            &spec.path,
            &spec.text,
            &spec.engine,
            &spec.allowed,
            &spec.required,
        )
        .map_err(|error| ConfigError::invalid_data(error.message))?;
        Ok(Self {
            id: id.to_owned(),
            compiled: Arc::new(compiled),
        })
    }
}

/// A compiled catalog in configuration/YAML insertion order, not map-sorted order.
///
/// The only constructors use the mandatory configuration callback to compile every
/// descriptor with the real engine. Failed validation never returns a partial catalog.
///
/// ```compile_fail
/// use parrot_foundation::template_catalog::PromptTemplateCatalog;
/// use parrot_domain::contracts::async_ops::Cancellation;
/// use minijinja::value::Value;
/// fn host_values(catalog: &PromptTemplateCatalog, cancellation: &dyn Cancellation) {
///     catalog.render("custom", &[("host".to_owned(), Value::from(1))], cancellation);
/// }
/// ```
///
/// Arbitrary host objects cannot enter through the catalog either:
///
/// ```compile_fail
/// use parrot_foundation::template_catalog::PromptTemplateCatalog;
/// use parrot_domain::contracts::async_ops::Cancellation;
/// struct HostObject;
/// fn host_object(catalog: &PromptTemplateCatalog, cancellation: &dyn Cancellation) {
///     catalog.render("custom", &[("host".to_owned(), HostObject)], cancellation);
/// }
/// ```
pub struct PromptTemplateCatalog {
    templates: Vec<Template>,
    indices: BTreeMap<String, usize>,
}

impl PromptTemplateCatalog {
    /// Layers and validates the full configuration, compiling each descriptor once.
    pub fn from_content(
        content: &str,
        environment: &BTreeMap<String, String>,
    ) -> Result<(Configuration, Self)> {
        let mut templates = Vec::new();
        let configuration = Configuration::from_content(content, environment, |spec| {
            templates.push(Template::compile(spec)?);
            Ok(())
        })?;
        Ok((configuration, Self::from_compiled(templates)))
    }

    /// Uses the real configuration load/provisioning path with an explicit caller environment.
    pub fn load(
        path: &Path,
        predefined_path: &Path,
        environment: &BTreeMap<String, String>,
    ) -> Result<(Configuration, Self)> {
        let mut templates = Vec::new();
        let configuration = Configuration::load(path, predefined_path, environment, |spec| {
            templates.push(Template::compile(spec)?);
            Ok(())
        })?;
        Ok((configuration, Self::from_compiled(templates)))
    }

    fn from_compiled(templates: Vec<Template>) -> Self {
        let indices = templates
            .iter()
            .enumerate()
            .map(|(index, template)| (template.id.clone(), index))
            .collect();
        Self { templates, indices }
    }

    /// Renders ordered argument pairs, rejecting duplicate names before map conversion.
    ///
    /// Cancellation is checked before validation and immediately before conversion.
    /// Native rendering retains the adapter's write/return cancellation checks and
    /// instruction/recursion budgets. No partial output is returned on any failure.
    pub fn render(
        &self,
        id: &str,
        arguments: &[(String, Value)],
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        let Some(index) = self.indices.get(id) else {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let mut seen = BTreeSet::new();
            for (name, _) in arguments {
                if !seen.insert(name) {
                    return Err(OperationError::Failed(ConfigError::invalid_data(format!(
                        "prompt_templates.{id} received duplicate argument '{name}'"
                    ))));
                }
            }
            return Err(OperationError::Failed(ConfigError::invalid_data(format!(
                "prompt_templates.{id} is not defined"
            ))));
        };
        self.templates[*index]
            .compiled
            .render(arguments, CancellationToken::new(cancellation))
            .map_err(translate_error)
    }

    /// Shares a compiled template without revalidating or rendering its inputs.
    pub fn get_template(&self, id: &str) -> Result<Arc<PromptTemplate>> {
        let index = self.indices.get(id).ok_or_else(|| {
            ConfigError::invalid_data(format!("prompt_templates.{id} is not defined"))
        })?;
        Ok(self.templates[*index].compiled.clone())
    }
}

fn translate_error(error: Error) -> OperationError<ConfigError> {
    if error.kind == ErrorKind::Cancelled {
        OperationError::Cancelled
    } else {
        OperationError::Failed(ConfigError::invalid_data(error.message))
    }
}
