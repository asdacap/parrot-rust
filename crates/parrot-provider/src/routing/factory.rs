//! Source implementation selection and concrete provider composition.
use super::{
    anthropic::{AnthropicApiKeySource, AnthropicBridge, map_provider_error},
    auth::CredentialSources,
    usage::{KimiUsageReporter, OpenCodeGoUsageReporter},
};
use crate::{
    anthropic::{AnthropicOptions, AnthropicProvider},
    catalogs::{self, Model, ModelCapabilities, ModelMetadataFields, ModelVariant},
    http_stream::HttpTransport,
    image_generation::ImageTransport,
    responses_websocket::Connector,
    sessions::{
        Content, Error, Provider,
        decorators::UsageReportingProvider,
        http::Protocol,
        provider::{
            CatalogDecoderSelection, ChatGptOptions, ChatGptProvider, CompatibleOptions,
            ImageTokenCalculator, OpenAICompatibleProvider,
        },
    },
};
use parrot_foundation::config::models::{ModelConfig, ProviderConfig};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderImplementation {
    Compatible,
    OpenAi,
    ChatGpt,
    OpenRouter,
    OpenCodeGo,
    KimiCode,
    KimiApi,
    Anthropic,
}
impl ProviderImplementation {
    pub fn resolve(id: &str, config: &ProviderConfig) -> Self {
        if config.provider_type == "anthropic" {
            return Self::Anthropic;
        }
        match id {
            "openai" => Self::OpenAi,
            "chatgpt" => Self::ChatGpt,
            "openrouter" => Self::OpenRouter,
            "opencode-go" => Self::OpenCodeGo,
            "kimi-code" => Self::KimiCode,
            "kimi-api" => Self::KimiApi,
            _ => Self::Compatible,
        }
    }
    pub fn can_build(self, config: &ProviderConfig) -> bool {
        self == Self::ChatGpt || !config.base_url.is_empty()
    }
}

pub struct ProviderBuildContext<'a> {
    pub id: &'a str,
    pub config: &'a ProviderConfig,
    pub credentials: &'a dyn CredentialSources,
    pub transport: Arc<dyn HttpTransport>,
    pub images: Arc<dyn ImageTransport>,
    pub connector: Arc<dyn Connector>,
    pub external_models: &'a [Model],
    pub maximum_request_bytes: usize,
}

pub struct BuiltProvider {
    pub provider: Arc<dyn Provider>,
    pub seed: Vec<Model>,
    pub implementation: ProviderImplementation,
}

pub fn build_provider(context: ProviderBuildContext<'_>) -> Result<BuiltProvider, Error> {
    let implementation = ProviderImplementation::resolve(context.id, context.config);
    let config = context.config;
    let models = read_declared(context.id, &config.models);
    let defaults = read_defaults(context.id, &config.model_defaults);
    let provider: Arc<dyn Provider> = match implementation {
        ProviderImplementation::ChatGpt => {
            let mut options = ChatGptOptions::new(context.credentials.oauth("chatgpt"));
            options.models = models;
            options.model_defaults = defaults;
            options.external_models = context.external_models.to_vec();
            options.disable_websocket = config.disable_websocket;
            options.maximum_request_bytes = context.maximum_request_bytes;
            options.header_timeout_ms = i64::from(config.header_timeout_ms);
            options.stream_idle_timeout_ms = i64::from(config.stream_idle_timeout_ms);
            Arc::new(ChatGptProvider::new_with_images(
                options,
                context.transport,
                context.images,
                context.connector,
            )?)
        }
        ProviderImplementation::Anthropic => {
            let keys = Arc::new(AnthropicApiKeySource::new(
                context.credentials.api_key(context.id, &config.api_key_env),
            ));
            let mut options = AnthropicOptions::new(context.id, &config.base_url, keys);
            options.headers = config
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            options.allow_insecure_localhost = config.allow_insecure_localhost;
            options.allow_insecure_remote = config.allow_insecure_remote;
            options.models = models;
            options.model_defaults = defaults;
            options.external_models = context.external_models.to_vec();
            options.maximum_request_bytes = context.maximum_request_bytes;
            options.header_timeout_ms = i64::from(config.header_timeout_ms);
            options.stream_idle_timeout_ms = i64::from(config.stream_idle_timeout_ms);
            let provider =
                AnthropicProvider::new(options, context.transport).map_err(map_provider_error)?;
            Arc::new(AnthropicBridge::new(Arc::new(provider)))
        }
        _ => {
            if !matches!(
                config.provider_type.as_str(),
                "" | "compatible" | "openai-compatible"
            ) {
                return Err(Error::Provider(format!(
                    "provider: unsupported provider type \"{}\" for \"{}\"",
                    config.provider_type, context.id
                )));
            }
            let protocol = match config.protocol.as_str() {
                "responses" => Protocol::Responses,
                "" | "chat-completions" => Protocol::ChatCompletions,
                value => {
                    return Err(Error::Provider(format!(
                        "provider: unsupported protocol \"{value}\" for \"{}\"",
                        context.id
                    )));
                }
            };
            let mut options = CompatibleOptions::new(
                context.id,
                &config.base_url,
                context.credentials.api_key(context.id, &config.api_key_env),
            );
            options.protocol = protocol;
            options.headers = config
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            options.allow_insecure_localhost = config.allow_insecure_localhost;
            options.allow_insecure_remote = config.allow_insecure_remote;
            options.disable_websocket = config.disable_websocket;
            options.maximum_request_bytes = context.maximum_request_bytes;
            options.header_timeout_ms = i64::from(config.header_timeout_ms);
            options.stream_idle_timeout_ms = i64::from(config.stream_idle_timeout_ms);
            options.models = models;
            options.model_defaults = defaults;
            options.external_models = context.external_models.to_vec();
            options.session_header.clone_from(&config.session_header);
            match implementation {
                ProviderImplementation::OpenAi => {
                    options.image_token_calculator = Arc::new(OpenAiImageTokens)
                }
                ProviderImplementation::OpenRouter => {
                    options.decoder =
                        CatalogDecoderSelection::Legacy(catalogs::OpenRouterModelDecoder::decode);
                    options
                        .provider_preferences
                        .clone_from(&config.provider_preferences);
                }
                ProviderImplementation::KimiCode => {
                    options.decoder =
                        CatalogDecoderSelection::Legacy(catalogs::KimiModelDecoder::decode)
                }
                _ => {}
            }
            let mut usage_options = CompatibleOptions::new(
                &options.id,
                &options.base_url,
                options.api_key_source.clone(),
            );
            usage_options.allow_insecure_localhost = options.allow_insecure_localhost;
            usage_options.allow_insecure_remote = options.allow_insecure_remote;
            usage_options.user_agent.clone_from(&options.user_agent);
            let usage_transport = context.transport.clone();
            let compatible: Arc<dyn Provider> =
                Arc::new(OpenAICompatibleProvider::new_with_images(
                    options,
                    context.transport,
                    context.images,
                    context.connector,
                )?);
            match implementation {
                ProviderImplementation::OpenCodeGo => Arc::new(UsageReportingProvider::new(
                    compatible,
                    Arc::new(OpenCodeGoUsageReporter::new(
                        &usage_options,
                        usage_transport,
                    )?),
                )),
                ProviderImplementation::KimiApi => Arc::new(UsageReportingProvider::new(
                    compatible,
                    Arc::new(KimiUsageReporter::new(&usage_options, usage_transport)?),
                )),
                _ => compatible,
            }
        }
    };
    let seed = provider.seed_models();
    Ok(BuiltProvider {
        provider,
        seed,
        implementation,
    })
}

struct OpenAiImageTokens;
impl ImageTokenCalculator for OpenAiImageTokens {
    fn calculate(&self, model: &Model, content: &Content) -> i64 {
        crate::image_inspection::calculate_content_tokens(
            &model.id,
            matches!(content, Content::Image { .. }),
            content.image_width(),
            content.image_height(),
        )
    }
}

pub fn read_declared(provider_id: &str, models: &BTreeMap<String, ModelConfig>) -> Vec<Model> {
    read_models(provider_id, models, true)
}
pub fn read_defaults(provider_id: &str, models: &BTreeMap<String, ModelConfig>) -> Vec<Model> {
    read_models(provider_id, models, false)
}
fn read_models(
    provider_id: &str,
    models: &BTreeMap<String, ModelConfig>,
    sort_variants: bool,
) -> Vec<Model> {
    models
        .iter()
        .map(|(id, config)| {
            let mut variants: Vec<_> = config
                .variants
                .iter()
                .map(|(name, effort)| ModelVariant::new(name, effort))
                .collect();
            if sort_variants {
                variants
                    .sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            }
            Model {
                id: id.clone(),
                provider_id: provider_id.into(),
                name: config.name.clone(),
                context_window: config.context,
                max_output_tokens: config.max_output_tokens,
                max_input_tokens: config.max_input_tokens,
                input_price: config.input_price,
                cached_input_price: config.cached_input_price,
                output_price: config.output_price,
                capabilities: ModelCapabilities {
                    tools: config.tools,
                    reasoning: config.reasoning || !variants.is_empty(),
                    output: if config.output.is_empty() {
                        vec!["text".into()]
                    } else {
                        config.output.clone()
                    },
                    variants,
                },
                fields: convert_fields(config.fields.0),
            }
        })
        .collect()
}
fn convert_fields(bits: u16) -> ModelMetadataFields {
    let fields = [
        ModelMetadataFields::NAME,
        ModelMetadataFields::CONTEXT_WINDOW,
        ModelMetadataFields::MAX_OUTPUT_TOKENS,
        ModelMetadataFields::INPUT_PRICE,
        ModelMetadataFields::CACHED_INPUT_PRICE,
        ModelMetadataFields::OUTPUT_PRICE,
        ModelMetadataFields::TOOLS,
        ModelMetadataFields::REASONING,
        ModelMetadataFields::OUTPUT,
        ModelMetadataFields::VARIANTS,
        ModelMetadataFields::MAX_INPUT_TOKENS,
    ];
    fields
        .into_iter()
        .enumerate()
        .filter(|(index, _)| bits & (1 << index) != 0)
        .fold(ModelMetadataFields::NONE, |mask, (_, field)| {
            mask.union(field)
        })
}
