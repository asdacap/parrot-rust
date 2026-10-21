//! Scalar/native-Jinja catalog composition using the real configuration validator.
//!
//! Descriptor and argument order are retained. Inputs are owned JSON values, not
//! host objects or engine values; scalar insertion accepts actual strings only.
use crate::config::{ConfigError, Configuration, Result, templates::TemplateSpec};
use crate::template_minijinja::MiniJinjaPromptTemplateEngine;
use crate::template_scalar::{ScalarPromptTemplateEngine, validate_arguments};
use parrot_domain::contracts::async_ops::{Cancellation, OperationError, OperationResult};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

enum Engine {
    Scalar(ScalarPromptTemplateEngine),
    MiniJinja(MiniJinjaPromptTemplateEngine),
}

struct Template {
    id: String,
    spec: TemplateSpec,
    allowed: BTreeSet<String>,
    engine: Engine,
}

impl Template {
    fn compile(spec: &TemplateSpec) -> Result<Self> {
        let id = spec.path.strip_prefix("prompt_templates.").ok_or_else(|| {
            ConfigError::invalid_data(format!("{} must be a prompt template path", spec.path))
        })?;
        let allowed = spec.allowed.iter().cloned().collect();
        let engine = match spec.engine.as_str() {
            "scalar" => Engine::Scalar(ScalarPromptTemplateEngine::compile(
                &spec.path, &spec.text, &allowed,
            )?),
            "minijinja" => Engine::MiniJinja(MiniJinjaPromptTemplateEngine::compile(
                &spec.path, &spec.text,
            )?),
            _ => {
                // The config reader owns discriminator diagnostics, including the
                // explicit manual-migration error for historical Scriban input.
                return Err(ConfigError::invalid_data(format!(
                    "{}.engine must be scalar or minijinja",
                    spec.path
                )));
            }
        };
        Ok(Self {
            id: id.to_owned(),
            spec: spec.clone(),
            allowed,
            engine,
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

    /// Enumerates the exact configured descriptors and their ordered argument inventories.
    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = (&str, &TemplateSpec)> {
        self.templates
            .iter()
            .map(|entry| (entry.id.as_str(), &entry.spec))
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
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        let names: Vec<_> = arguments.iter().map(|(name, _)| name.clone()).collect();
        let Some(index) = self.indices.get(id) else {
            // Source list-based Render detects duplicates before looking up the ID.
            let mut seen = BTreeSet::new();
            for name in &names {
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
        let template = &self.templates[*index];
        validate_arguments(
            &template.spec.path,
            &template.allowed,
            &template.spec.required,
            &names,
        )
        .map_err(OperationError::Failed)?;
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        match &template.engine {
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

    pub fn render_skills(
        &self,
        skills: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        self.render(
            "context.skills",
            &[("skills".into(), Value::String(skills.into()))],
            cancellation,
        )
    }

    pub fn render_selected_skill(
        &self,
        name: &str,
        path: &str,
        content: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        self.render(
            "context.skill-selected",
            &[
                ("name".into(), Value::String(name.into())),
                ("path".into(), Value::String(path.into())),
                ("content".into(), Value::String(content.into())),
            ],
            cancellation,
        )
    }

    pub fn render_unavailable_skill(
        &self,
        name: &str,
        message: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<String, ConfigError> {
        self.render(
            "context.skill-unavailable",
            &[
                ("name".into(), Value::String(name.into())),
                ("message".into(), Value::String(message.into())),
            ],
            cancellation,
        )
    }
}
