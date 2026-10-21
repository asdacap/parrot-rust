//! Model configuration transactions and publication of validated routing generations.
use super::aliases::{
    ModelAliasDefinition, ModelAliasIcon, ModelAliasIconError, ModelAliasSnapshot, ModelRouter,
    ModelRouting, ModelRoutingSnapshot, ResolvedModelSelection,
};
use crate::sessions::Error;
use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::{
    ConfigError, Configuration,
    configuration::{ModelConfigurationSnapshot, ModelConfigurationTransaction},
    models::{
        ModelAliasConfig, ModelPresetConfig, ModelPresetContextLimits, ProviderModelAliasDefaults,
        validate_preset_name,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinatorError {
    Foundation(ConfigError),
    Provider(Error),
    Icon(ModelAliasIconError),
    MissingPreset(String),
}
impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foundation(error) => error.fmt(formatter),
            Self::Provider(error) => error.fmt(formatter),
            Self::Icon(error) => error.fmt(formatter),
            Self::MissingPreset(name) => write!(
                formatter,
                "model preset: preset \"{name}\" is not configured"
            ),
        }
    }
}
impl std::error::Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Foundation(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::Icon(error) => Some(error),
            Self::MissingPreset(_) => None,
        }
    }
}
impl From<ConfigError> for CoordinatorError {
    fn from(error: ConfigError) -> Self {
        Self::Foundation(error)
    }
}
impl From<Error> for CoordinatorError {
    fn from(error: Error) -> Self {
        Self::Provider(error)
    }
}
impl From<ModelAliasIconError> for CoordinatorError {
    fn from(error: ModelAliasIconError) -> Self {
        Self::Icon(error)
    }
}
pub type Result<T> = std::result::Result<T, CoordinatorError>;

pub fn definitions_from_config(
    aliases: &BTreeMap<String, ModelAliasConfig>,
) -> Result<Vec<ModelAliasDefinition>> {
    let mut definitions = aliases
        .iter()
        .map(|(name, alias)| {
            Ok(ModelAliasDefinition {
                name: name.clone(),
                model_string: alias.model_string.clone(),
                usage: alias.usage.clone(),
                augment_system_prompt: alias.augment_system_prompt.clone(),
                icon: alias
                    .icon
                    .as_ref()
                    .map(|icon| ModelAliasIcon::parse(&icon.glyph, &icon.color))
                    .transpose()?,
                context_limit: alias.context_limit.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    definitions.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
    Ok(definitions)
}

#[derive(Clone, Debug)]
pub struct ModelPresetSelection {
    pub preset: ModelPresetConfig,
    pub resolved_selection: ResolvedModelSelection,
}

pub struct ModelConfigurationCoordinator {
    configuration: Mutex<Configuration>,
    routing: Arc<ModelRouting>,
    router: Arc<ModelRouter>,
}
impl ModelConfigurationCoordinator {
    pub fn new(
        configuration: Configuration,
        routing: Arc<ModelRouting>,
        router: Arc<ModelRouter>,
    ) -> Self {
        Self {
            configuration: Mutex::new(configuration),
            routing,
            router,
        }
    }
    pub fn capture_configuration(&self) -> Configuration {
        self.configuration
            .lock()
            .expect("model configuration state lock")
            .clone()
    }
    pub fn capture_routing(&self) -> Arc<ModelRoutingSnapshot> {
        self.routing.capture()
    }
    pub fn capture_presets(&self) -> BTreeMap<String, ModelPresetConfig> {
        self.configuration
            .lock()
            .expect("model configuration state lock")
            .models
            .model_presets
            .clone()
    }
    fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut ModelConfigurationTransaction<'_>) -> Result<T>,
    ) -> Result<T> {
        // The Foundation clone shares its automatic per-path transaction gate. The
        // materialized-state mutex is never held while entering that gate or a callback.
        let mut working = self.capture_configuration();
        working.model_configuration_transaction(operation)?
    }
    fn synchronize(&self, transaction: &ModelConfigurationTransaction<'_>) {
        self.configuration
            .lock()
            .expect("model configuration state lock")
            .synchronize_model_configuration(&transaction.snapshot());
    }
    fn prepare(
        &self,
        configuration: &ModelConfigurationSnapshot,
    ) -> Result<Arc<ModelRoutingSnapshot>> {
        let mut prepared = (*self.routing.prepare(
            &configuration.model,
            definitions_from_config(&configuration.model_aliases)?,
        )?)
        .clone();
        prepared.context_limit = configuration.context_limit.clone();
        Ok(Arc::new(prepared))
    }
    fn reload(
        &self,
        transaction: &mut ModelConfigurationTransaction<'_>,
    ) -> Result<ModelConfigurationSnapshot> {
        let refreshed = transaction.reload()?;
        let replacement = self.prepare(&refreshed)?;
        transaction.synchronize(&refreshed);
        self.synchronize(transaction);
        self.routing.publish(replacement);
        Ok(refreshed)
    }
    pub fn configure_alias(&self, name: &str, model_string: &str) -> Result<ModelAliasDefinition> {
        self.transaction(|transaction| {
            let refreshed = self.reload(transaction)?;
            let existing = refreshed.model_aliases.get(name).ok_or_else(|| {
                Error::Provider(format!("model alias: alias \"{name}\" is not defined"))
            })?;
            let definitions = definitions_from_config(&refreshed.model_aliases)?
                .into_iter()
                .map(|mut definition| {
                    if definition.name == name {
                        definition.model_string = model_string.to_owned();
                    }
                    definition
                })
                .collect::<Vec<_>>();
            let mut replacement = (*self.routing.prepare(&refreshed.model, definitions)?).clone();
            replacement.context_limit = refreshed.context_limit.clone();
            transaction.set_model_alias(name, model_string)?;
            self.synchronize(transaction);
            self.routing.publish(Arc::new(replacement));
            Ok(ModelAliasDefinition {
                name: name.to_owned(),
                model_string: model_string.to_owned(),
                usage: existing.usage.clone(),
                augment_system_prompt: existing.augment_system_prompt.clone(),
                icon: existing
                    .icon
                    .as_ref()
                    .map(|icon| ModelAliasIcon::parse(&icon.glyph, &icon.color))
                    .transpose()?,
                context_limit: existing.context_limit.clone(),
            })
        })
    }
    pub fn list_provider_defaults<I, S>(&self, provider_ids: I) -> Vec<ProviderModelAliasDefaults>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let configuration = self.capture_configuration();
        let ids = provider_ids
            .into_iter()
            .map(|id| id.as_ref().to_owned())
            .collect::<BTreeSet<_>>();
        let mut defaults = ids
            .into_iter()
            .filter_map(|id| {
                configuration
                    .models
                    .provider_model_alias_defaults
                    .get(&id)
                    .cloned()
            })
            .collect::<Vec<_>>();
        defaults.sort_by(|left, right| {
            left.provider_id
                .encode_utf16()
                .cmp(right.provider_id.encode_utf16())
        });
        defaults
    }
    pub fn apply_provider_defaults(&self, provider_id: &str) -> Result<Vec<ModelAliasDefinition>> {
        self.transaction(|transaction| {
            let refreshed = self.reload(transaction)?;
            let defaults = self
                .capture_configuration()
                .models
                .provider_model_alias_defaults
                .get(provider_id)
                .cloned()
                .ok_or_else(|| {
                    Error::Provider(format!(
                        "model alias defaults: provider \"{provider_id}\" is not configured"
                    ))
                })?;
            let targets = defaults.targets();
            let definitions = definitions_from_config(&refreshed.model_aliases)?
                .into_iter()
                .map(|mut definition| {
                    if let Some(target) = targets.get(&definition.name) {
                        definition.model_string.clone_from(target);
                    }
                    definition
                })
                .collect::<Vec<_>>();
            let mut replacement = (*self
                .routing
                .prepare(&refreshed.model, definitions.clone())?)
            .clone();
            replacement.context_limit = refreshed.context_limit;
            transaction.set_model_aliases(&defaults)?;
            self.synchronize(transaction);
            self.routing.publish(Arc::new(replacement));
            Ok(definitions
                .into_iter()
                .filter(|definition| targets.contains_key(&definition.name))
                .collect())
        })
    }
    pub fn set_preset(&self, name: &str, selected_model: &str) -> Result<ModelPresetConfig> {
        self.transaction(|transaction| {
            validate_preset_name(name)?;
            let refreshed = self.reload(transaction)?;
            self.router.resolve(selected_model)?;
            let preset = ModelPresetConfig {
                model: selected_model.to_owned(),
                model_aliases: refreshed
                    .model_aliases
                    .iter()
                    .map(|(name, alias)| (name.clone(), alias.model_string.clone()))
                    .collect(),
                context_limits: Some(context_limits(&refreshed)),
            };
            transaction.set_model_preset(name, &preset)?;
            self.synchronize(transaction);
            Ok(preset)
        })
    }
    pub fn select_preset(
        &self,
        name: &str,
        selected_model: &str,
        mut apply_selection: impl FnMut(ResolvedModelSelection) -> Result<()>,
    ) -> Result<ModelPresetSelection> {
        self.transaction(|transaction| {
            validate_preset_name(name)?;
            let refreshed = self.reload(transaction)?;
            let previous_configuration = transaction.reload()?;
            let previous_routing = self.routing.capture();
            let previous_selection = self.router.resolve_from(previous_routing.clone(), selected_model)?;
            let preset = refreshed.model_presets.get(name).cloned().ok_or_else(|| CoordinatorError::MissingPreset(name.to_owned()))?;
            let mut aliases = refreshed.model_aliases.clone();
            for (alias_name, target) in &preset.model_aliases {
                let alias = aliases.get_mut(alias_name).ok_or_else(|| Error::Provider(format!("model preset: alias \"{alias_name}\" from preset \"{name}\" is no longer defined")))?;
                alias.model_string.clone_from(target);
            }
            if let Some(limits) = &preset.context_limits {
                for alias_name in limits.model_aliases.keys() {
                    if !aliases.contains_key(alias_name) { return Err(Error::Provider(format!("model preset: alias \"{alias_name}\" is no longer defined")).into()); }
                }
                for (alias_name, alias) in &mut aliases { alias.context_limit = limits.model_aliases.get(alias_name).cloned(); }
            }
            let mut replacement = (*self.routing.prepare(&preset.model, definitions_from_config(&aliases)?)?).clone();
            replacement.context_limit = preset.context_limits.as_ref().map_or_else(|| refreshed.context_limit.clone(), |limits| limits.default.clone());
            let replacement = Arc::new(replacement);
            let resolved = self.router.resolve_from(replacement.clone(), &preset.model)?;
            transaction.set_model_routing_with_limits(&preset.model, &preset.model_aliases, preset.context_limits.as_ref())?;
            self.synchronize(transaction);
            self.routing.publish(replacement);
            if let Err(original) = apply_selection(resolved.clone()) {
                let restored_selection = apply_selection(previous_selection);
                self.rollback(transaction, &previous_configuration, previous_routing)?;
                return Err(restored_selection.err().unwrap_or(original));
            }
            Ok(ModelPresetSelection { preset, resolved_selection: resolved })
        })
    }
    fn rollback(
        &self,
        transaction: &mut ModelConfigurationTransaction<'_>,
        previous: &ModelConfigurationSnapshot,
        routing: Arc<ModelRoutingSnapshot>,
    ) -> Result<()> {
        transaction.synchronize(previous);
        let targets = previous
            .model_aliases
            .iter()
            .map(|(name, alias)| (name.clone(), alias.model_string.clone()))
            .collect();
        transaction.set_model_routing_with_limits(
            &previous.model,
            &targets,
            Some(&context_limits(previous)),
        )?;
        self.synchronize(transaction);
        self.routing.publish(routing);
        Ok(())
    }
    pub fn set_context_limit(
        &self,
        context_limit: ContextSize,
    ) -> Result<Arc<ModelRoutingSnapshot>> {
        self.transaction(|transaction| {
            let refreshed = self.reload(transaction)?;
            let mut replacement = (*self.routing.prepare(
                &refreshed.model,
                definitions_from_config(&refreshed.model_aliases)?,
            )?)
            .clone();
            replacement.context_limit = Some(context_limit.clone());
            let replacement = Arc::new(replacement);
            transaction.set_context_limit(context_limit)?;
            self.synchronize(transaction);
            self.routing.publish(replacement.clone());
            Ok(replacement)
        })
    }
    pub fn refresh(&self) -> Result<Arc<ModelRoutingSnapshot>> {
        self.transaction(|transaction| {
            self.reload(transaction)?;
            Ok(self.routing.capture())
        })
    }
}
fn context_limits(configuration: &ModelConfigurationSnapshot) -> ModelPresetContextLimits {
    ModelPresetContextLimits {
        default: configuration.context_limit.clone(),
        model_aliases: configuration
            .model_aliases
            .iter()
            .filter_map(|(name, alias)| {
                alias
                    .context_limit
                    .as_ref()
                    .map(|limit| (name.clone(), limit.clone()))
            })
            .collect(),
    }
}

pub struct ModelAliasConfigurator {
    models: Arc<ModelConfigurationCoordinator>,
}
impl ModelAliasConfigurator {
    pub fn new(models: Arc<ModelConfigurationCoordinator>) -> Self {
        Self { models }
    }
    pub fn capture(&self) -> Arc<ModelAliasSnapshot> {
        self.models.capture_routing().aliases.clone()
    }
    pub fn configure(&self, name: &str, model_string: &str) -> Result<ModelAliasDefinition> {
        self.models.configure_alias(name, model_string)
    }
    pub fn list_provider_defaults<I, S>(&self, provider_ids: I) -> Vec<ProviderModelAliasDefaults>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.models.list_provider_defaults(provider_ids)
    }
    pub fn apply_provider_defaults(&self, provider_id: &str) -> Result<Vec<ModelAliasDefinition>> {
        self.models.apply_provider_defaults(provider_id)
    }
}
