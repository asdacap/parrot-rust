//! Model RPC operations over the authoritative provider routing/configuration objects.
use super::{Result, RpcCode, RpcError};
use parrot_domain::{
    contracts::async_ops::Cancellation, protocol::messages::parrot as proto,
    values::context::ContextSize,
};
use parrot_foundation::config::{ConfigErrorKind, models::ModelPresetConfig};
use parrot_provider::{
    routing::{
        aliases::{ModelAliasDefinition, ModelRouter, ResolvedModelSelection},
        configuration::{
            CoordinatorError, ModelAliasConfigurator, ModelConfigurationCoordinator,
            ModelPresetSelection,
        },
        registry::ProviderRegistry,
    },
    sessions::{Error as ProviderError, decorators::SubscriptionUsage},
};
use std::sync::Arc;

pub struct ModelOperations {
    configuration: Arc<ModelConfigurationCoordinator>,
    registry: Arc<ProviderRegistry>,
    aliases: ModelAliasConfigurator,
    router: Arc<ModelRouter>,
}

impl ModelOperations {
    /// All arguments must be the same instances used by the host composition root.
    pub fn new(
        configuration: Arc<ModelConfigurationCoordinator>,
        registry: Arc<ProviderRegistry>,
        router: Arc<ModelRouter>,
    ) -> Self {
        Self {
            aliases: ModelAliasConfigurator::new(configuration.clone()),
            configuration,
            registry,
            router,
        }
    }

    pub fn resolve(&self, selector: &str) -> Result<ResolvedModelSelection> {
        self.router
            .resolve(selector)
            .map_err(|error| provider_error(error, "failed to resolve model"))
    }

    pub async fn list_models(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::ListModelsResponse> {
        let available = self
            .registry
            .available_models(cancellation)
            .await
            .map_err(|error| provider_error(error, "failed to list models"))?;
        Ok(proto::ListModelsResponse {
            models: available
                .into_iter()
                .map(|entry| proto::Model {
                    id: entry.model.id,
                    provider_id: entry.model.provider_id,
                    variants: entry
                        .model
                        .capabilities
                        .variants
                        .into_iter()
                        .map(|variant| proto::ModelVariant {
                            name: variant.name,
                            reasoning_effort: variant.reasoning_effort,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    pub fn list_model_aliases(&self) -> proto::ListModelAliasesResponse {
        proto::ListModelAliasesResponse {
            aliases: self
                .aliases
                .capture()
                .definitions()
                .iter()
                .map(alias_to_protocol)
                .collect(),
            ..Default::default()
        }
    }

    pub fn configure_model_alias(
        &self,
        request: &proto::ConfigureModelAliasRequest,
    ) -> Result<proto::ConfigureModelAliasResponse> {
        let alias = self
            .aliases
            .configure(&request.name, &request.model_string)
            .map_err(|error| configuration_error(error, "failed to persist model alias"))?;
        Ok(proto::ConfigureModelAliasResponse {
            alias: Some(alias_to_protocol(&alias)).into(),
            ..Default::default()
        })
    }

    pub async fn list_provider_model_alias_defaults(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::ListProviderModelAliasDefaultsResponse> {
        let available = self
            .registry
            .available_models(cancellation)
            .await
            .map_err(|error| {
                provider_error(error, "failed to list provider model alias defaults")
            })?;
        Ok(proto::ListProviderModelAliasDefaultsResponse {
            providers: self
                .aliases
                .list_provider_defaults(available.iter().map(|model| model.provider.id()))
                .into_iter()
                .map(|defaults| proto::ProviderModelAliasDefaults {
                    provider_id: defaults.provider_id,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    pub async fn apply_provider_model_alias_defaults(
        &self,
        request: &proto::ApplyProviderModelAliasDefaultsRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::ApplyProviderModelAliasDefaultsResponse> {
        let available = self
            .list_provider_model_alias_defaults(cancellation)
            .await?;
        if !available
            .providers
            .iter()
            .any(|provider| provider.provider_id == request.provider_id)
        {
            return Err(RpcError::new(
                RpcCode::InvalidArgument,
                format!(
                    "model alias defaults: provider \"{}\" is not available",
                    request.provider_id
                ),
            ));
        }
        let aliases = self
            .aliases
            .apply_provider_defaults(&request.provider_id)
            .map_err(|error| configuration_error(error, "failed to persist model aliases"))?;
        Ok(proto::ApplyProviderModelAliasDefaultsResponse {
            aliases: aliases.iter().map(alias_to_protocol).collect(),
            ..Default::default()
        })
    }

    pub fn list_model_presets(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::ListModelPresetsResponse> {
        check_cancellation(cancellation)?;
        Ok(proto::ListModelPresetsResponse {
            presets: self
                .configuration
                .capture_presets()
                .iter()
                .map(|(name, preset)| preset_to_protocol(name, preset))
                .collect(),
            ..Default::default()
        })
    }

    pub fn set_model_preset(
        &self,
        request: &proto::SetModelPresetRequest,
        current_model: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::SetModelPresetResponse> {
        check_cancellation(cancellation)?;
        let preset = self
            .configuration
            .set_preset(&request.name, current_model)
            .map_err(|error| configuration_error(error, "failed to persist model preset"))?;
        Ok(proto::SetModelPresetResponse {
            preset: Some(preset_to_protocol(&request.name, &preset)).into(),
            ..Default::default()
        })
    }

    /// The host supplies its actual session selection update, including rollback updates.
    /// It maps the current session only after this transaction has completed successfully.
    pub fn select_model_preset(
        &self,
        request: &proto::SelectModelPresetRequest,
        current_model: &str,
        apply_selection: impl FnMut(
            ResolvedModelSelection,
        ) -> parrot_provider::routing::configuration::Result<()>,
        cancellation: &dyn Cancellation,
    ) -> Result<(proto::ModelPreset, ModelPresetSelection)> {
        check_cancellation(cancellation)?;
        let selected = self
            .configuration
            .select_preset(&request.name, current_model, apply_selection)
            .map_err(|error| configuration_error(error, "failed to select model preset"))?;
        Ok((
            preset_to_protocol(&request.name, &selected.preset),
            selected,
        ))
    }

    pub fn set_context_limit(
        &self,
        request: &proto::SetContextLimitRequest,
        current_model: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<proto::SetContextLimitResponse> {
        check_cancellation(cancellation)?;
        if request.context_limit.is_empty() {
            return Err(RpcError::new(
                RpcCode::InvalidArgument,
                "context limit must not be empty",
            ));
        }
        let limit = ContextSize::parse(&request.context_limit)
            .map_err(|error| RpcError::new(RpcCode::InvalidArgument, error.to_string()))?;
        self.configuration
            .set_context_limit(limit.clone())
            .map_err(|error| match error {
                CoordinatorError::Foundation(error) if error.kind() == ConfigErrorKind::Io => {
                    RpcError::unhandled_io(error.to_string())
                }
                CoordinatorError::Foundation(error) => {
                    RpcError::new(RpcCode::InvalidArgument, error.to_string())
                }
                error => configuration_error(error, "failed to persist context limit"),
            })?;
        let alias_override = !current_model.is_empty()
            && self
                .configuration
                .capture_routing()
                .aliases
                .find(current_model)
                .is_some_and(|alias| alias.context_limit.is_some());
        Ok(proto::SetContextLimitResponse {
            context_limit: limit.to_string(),
            alias_override,
            ..Default::default()
        })
    }

    pub async fn usage_lines(
        &self,
        resolved: &ResolvedModelSelection,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<String>> {
        let reporter = self
            .registry
            .list()
            .into_iter()
            .find(|provider| provider.id() == resolved.canonical_model.provider.id())
            .and_then(|provider| provider.usage_reporter());
        let Some(reporter) = reporter else {
            return Ok(vec!["Provider does not report usage".into()]);
        };
        match reporter.usage(cancellation).await {
            Ok(usage) => {
                Ok(format_usage(usage).unwrap_or_else(|| vec!["Usage unavailable".into()]))
            }
            Err(ProviderError::Cancelled | ProviderError::Cancellation(_))
                if cancellation.is_cancelled() =>
            {
                Err(RpcError::new(RpcCode::Cancelled, "operation cancelled"))
            }
            Err(_) => Ok(vec!["Usage unavailable".into()]),
        }
    }
}

fn check_cancellation(cancellation: &dyn Cancellation) -> Result<()> {
    cancellation
        .check()
        .map_err(|error| RpcError::new(RpcCode::Cancelled, error.to_string()))
}

fn alias_to_protocol(alias: &ModelAliasDefinition) -> proto::ModelAlias {
    proto::ModelAlias {
        name: alias.name.clone(),
        model_string: alias.model_string.clone(),
        usage: alias.usage.clone(),
        ..Default::default()
    }
}

fn preset_to_protocol(name: &str, preset: &ModelPresetConfig) -> proto::ModelPreset {
    let mut converted = proto::ModelPreset {
        name: name.to_owned(),
        model: preset.model.clone(),
        model_aliases: preset.model_aliases.clone().into_iter().collect(),
        ..Default::default()
    };
    if let Some(limits) = &preset.context_limits {
        converted.context_limit = limits
            .default
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        converted.model_alias_context_limits = limits
            .model_aliases
            .iter()
            .map(|(name, limit)| (name.clone(), limit.to_string()))
            .collect();
    }
    converted
}

fn configuration_error(error: CoordinatorError, persistence: &str) -> RpcError {
    match error {
        CoordinatorError::MissingPreset(_) => RpcError::new(RpcCode::NotFound, error.to_string()),
        CoordinatorError::Provider(error) => provider_error(error, persistence),
        CoordinatorError::Foundation(ref failure) if failure.kind() == ConfigErrorKind::Io => {
            RpcError::new(RpcCode::Internal, format!("{persistence}: {error}"))
        }
        CoordinatorError::Foundation(_) | CoordinatorError::Icon(_) => {
            RpcError::new(RpcCode::InvalidArgument, error.to_string())
        }
    }
}

fn provider_error(error: ProviderError, persistence: &str) -> RpcError {
    match error {
        ProviderError::Cancelled | ProviderError::Cancellation(_) => {
            RpcError::new(RpcCode::Cancelled, error.to_string())
        }
        ProviderError::Provider(_) | ProviderError::ProviderWithCause { .. } => {
            RpcError::new(RpcCode::InvalidArgument, error.to_string())
        }
        ProviderError::Io(_) => RpcError::new(RpcCode::Internal, format!("{persistence}: {error}")),
        _ => RpcError::new(RpcCode::Internal, error.to_string()),
    }
}

fn format_usage(usage: SubscriptionUsage) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    for (label, window) in [
        ("Primary", usage.primary_window),
        ("Secondary", usage.secondary_window),
    ] {
        if let Some(window) = window {
            let reset = chrono::DateTime::parse_from_rfc3339(&window.reset_at).ok()?;
            lines.push(format!(
                "{label} window: {:.0}% used, resets {}",
                window.used_percent.round(),
                reset.format("%Y-%m-%dT%H:%M:%SZ")
            ));
        }
    }
    if let Some(credits) = usage.credits.filter(|credits| credits.has_credits) {
        lines.push(format!("Credits: {}", credits.balance));
    }
    if lines.is_empty() {
        lines.push("Usage unavailable".into());
    }
    Some(lines)
}
