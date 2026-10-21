//! Model/provider configuration readers from the pinned Foundation configuration.

use super::node::{
    Kind, Node, Style, boolean, mapping, nonnegative_integer, positive_integer, scalar_or_empty,
    validate_keys,
};
use super::{ConfigError, Result};
use parrot_domain::values::config::{ModelProfileConfig, ModelProfiles};
use parrot_domain::values::context::ContextSize;
use std::collections::BTreeMap;
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasIconConfig {
    pub glyph: String,
    pub color: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasConfig {
    pub model_string: String,
    pub usage: String,
    pub augment_system_prompt: Option<String>,
    pub icon: Option<ModelAliasIconConfig>,
    pub context_limit: Option<ContextSize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPresetContextLimits {
    pub default: Option<ContextSize>,
    pub model_aliases: BTreeMap<String, ContextSize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPresetConfig {
    pub model: String,
    pub model_aliases: BTreeMap<String, String>,
    pub context_limits: Option<ModelPresetContextLimits>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderModelAliasDefaults {
    pub provider_id: String,
    pub low_model_string: String,
    pub medium_model_string: String,
    pub high_model_string: String,
    pub xhigh_model_string: String,
}

impl ProviderModelAliasDefaults {
    pub fn targets(&self) -> BTreeMap<String, String> {
        [
            ("low_llm", &self.low_model_string),
            ("medium_llm", &self.medium_model_string),
            ("high_llm", &self.high_model_string),
            ("xhigh_llm", &self.xhigh_model_string),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.clone()))
        .collect()
    }
}

/// Source ModelConfigFields bit positions, including MaxInputTokens at bit ten.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelConfigFields(pub u16);

impl ModelConfigFields {
    pub const NAME: u16 = 1;
    pub const CONTEXT: u16 = 1 << 1;
    pub const MAX_OUTPUT_TOKENS: u16 = 1 << 2;
    pub const INPUT_PRICE: u16 = 1 << 3;
    pub const CACHED_INPUT_PRICE: u16 = 1 << 4;
    pub const OUTPUT_PRICE: u16 = 1 << 5;
    pub const TOOLS: u16 = 1 << 6;
    pub const REASONING: u16 = 1 << 7;
    pub const OUTPUT: u16 = 1 << 8;
    pub const VARIANTS: u16 = 1 << 9;
    pub const MAX_INPUT_TOKENS: u16 = 1 << 10;
    pub fn contains(self, field: u16) -> bool {
        self.0 & field == field
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelConfig {
    pub name: String,
    pub context: i32,
    pub max_output_tokens: i32,
    pub max_input_tokens: i32,
    pub input_price: f64,
    pub cached_input_price: f64,
    pub output_price: f64,
    pub tools: bool,
    pub reasoning: bool,
    pub output: Vec<String>,
    pub variants: BTreeMap<String, String>,
    pub fields: ModelConfigFields,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderConfig {
    pub provider_type: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key_env: String,
    pub headers: BTreeMap<String, String>,
    pub allow_insecure_localhost: bool,
    pub allow_insecure_remote: bool,
    pub allow_invalid_tls_certificate: bool,
    pub disable_websocket: bool,
    pub image_urls: bool,
    pub stream_idle_timeout_ms: i32,
    pub header_timeout_ms: i32,
    pub header_timeout_max_retries: i32,
    pub provider_preferences: String,
    pub session_header: String,
    pub models_dev_id: String,
    pub model_defaults: BTreeMap<String, ModelConfig>,
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelSettings {
    pub model_aliases: BTreeMap<String, ModelAliasConfig>,
    pub model_presets: BTreeMap<String, ModelPresetConfig>,
    pub provider_model_alias_defaults: BTreeMap<String, ProviderModelAliasDefaults>,
    pub model_profiles: ModelProfiles,
    pub model_profile_entries: BTreeMap<String, ModelProfileConfig>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub context_limit: Option<ContextSize>,
}

impl ModelSettings {
    pub fn read(root: &Node) -> Result<Self> {
        let context_limit = read_context_limit(root, "context_limit", "context_limit")?;
        let model_aliases = read_model_aliases(root)?;
        let model_presets = read_model_presets(root)?;
        let provider_model_alias_defaults = read_provider_model_alias_defaults(root)?;
        let model_profile_entries = read_model_profiles(root)?;
        Ok(Self {
            context_limit,
            model_aliases,
            model_presets,
            provider_model_alias_defaults,
            model_profiles: ModelProfiles::new(&model_profile_entries),
            model_profile_entries,
            providers: read_providers(root)?,
        })
    }

    pub fn snapshot(&self) -> serde_json::Value {
        use serde_json::{Map, Value, json};
        let aliases: Map<String, Value> = self.model_aliases.iter().map(|(name, value)| (name.clone(), json!({
            "modelString": value.model_string, "usage": value.usage,
            "augmentSystemPrompt": value.augment_system_prompt,
            "icon": value.icon.as_ref().map(|icon| json!({"glyph":icon.glyph,"color":icon.color})),
            "contextLimit": value.context_limit.as_ref().map(ToString::to_string),
        }))).collect();
        let presets: Map<String, Value> = self
            .model_presets
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    json!({
                        "model":value.model, "modelAliases":value.model_aliases,
                        "contextLimits":value.context_limits.as_ref().map(context_limits_snapshot),
                    }),
                )
            })
            .collect();
        let defaults: Map<String, Value> = self.provider_model_alias_defaults.iter().map(|(name,value)| (name.clone(),json!({
            "providerId":value.provider_id,"lowModelString":value.low_model_string,"mediumModelString":value.medium_model_string,"highModelString":value.high_model_string,"xHighModelString":value.xhigh_model_string,
        }))).collect();
        let profiles: Map<String, Value> = self.model_profile_entries.iter().map(|(name,value)| (name.clone(),json!({"usage":value.usage,"systemPrompt":value.system_prompt,"maximumInputTokens":value.maximum_input_tokens,"summaryOutputTokens":value.summary_output_tokens}))).collect();
        let providers: Map<String, Value> = self.providers.iter().map(|(name,value)| (name.clone(),json!({
            "type":value.provider_type,"protocol":value.protocol,"baseUrl":value.base_url,"apiKeyEnv":value.api_key_env,"headers":value.headers,
            "allowInsecureLocalhost":value.allow_insecure_localhost,"allowInsecureRemote":value.allow_insecure_remote,"allowInvalidTlsCertificate":value.allow_invalid_tls_certificate,
            "disableWebSocket":value.disable_websocket,"imageUrls":value.image_urls,"streamIdleTimeoutMs":value.stream_idle_timeout_ms,"headerTimeoutMs":value.header_timeout_ms,"headerTimeoutMaxRetries":value.header_timeout_max_retries,
            "providerPreferences":value.provider_preferences,"sessionHeader":value.session_header,"modelsDevId":value.models_dev_id,
            "modelDefaults":models_snapshot(&value.model_defaults),"models":models_snapshot(&value.models),
        }))).collect();
        json!({"modelAliases":aliases,"modelPresets":presets,"providerModelAliasDefaults":defaults,"modelProfiles":profiles,"providers":providers,"contextLimit":self.context_limit.as_ref().map(ToString::to_string)})
    }

    pub fn capture_context_limits(&self) -> ModelPresetContextLimits {
        ModelPresetContextLimits {
            default: self.context_limit.clone(),
            model_aliases: self
                .model_aliases
                .iter()
                .filter_map(|(name, alias)| {
                    alias
                        .context_limit
                        .clone()
                        .map(|limit| (name.clone(), limit))
                })
                .collect(),
        }
    }
}

pub fn validate_alias_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::invalid_data(
            "model_aliases key must not be empty".into(),
        ));
    }
    if name.trim() != name {
        return Err(ConfigError::invalid_data(format!(
            "model alias name \"{name}\" must not have surrounding whitespace"
        )));
    }
    if name.contains('/') {
        return Err(ConfigError::invalid_data(format!(
            "model alias name \"{name}\" must not contain '/'"
        )));
    }
    Ok(())
}

pub fn validate_preset_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.contains('/')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigError::invalid_data(
            "model_presets name must be one non-empty token without whitespace or '/'".into(),
        ));
    }
    Ok(())
}

pub fn validate_requested_selector(field: &str, selector: &str) -> Result<()> {
    if selector.is_empty()
        || selector.trim() != selector
        || selector
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || selector.split('/').any(str::is_empty)
    {
        return Err(ConfigError::invalid_data(format!(
            "{field} must be a model alias or provider/model selector"
        )));
    }
    Ok(())
}

pub fn validate_model_selector(field: &str, selector: &str, allow_empty: bool) -> Result<()> {
    if selector.is_empty() && allow_empty {
        return Ok(());
    }
    if selector.trim() != selector {
        return Err(ConfigError::invalid_data(format!(
            "{field} must not have surrounding whitespace"
        )));
    }
    if selector
        .chars()
        .any(|c| c.is_control() && c.is_whitespace())
    {
        return Err(ConfigError::invalid_data(format!(
            "{field} must not contain control whitespace"
        )));
    }
    if !selector.contains('/') {
        return Err(ConfigError::invalid_data(format!(
            "{field} must be provider/model"
        )));
    }
    if selector.split('/').any(str::is_empty) {
        return Err(ConfigError::invalid_data(format!(
            "{field} must not contain empty path segments"
        )));
    }
    Ok(())
}

fn required_scalar(parent: &Node, key: &str, field: &str) -> Result<String> {
    parent
        .get(key)
        .and_then(Node::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ConfigError::invalid_data(format!("{field} must be a string")))
}

pub fn read_context_limit(fields: &Node, key: &str, path: &str) -> Result<Option<ContextSize>> {
    let Some(node) = fields.get(key) else {
        return Ok(None);
    };
    if matches!(&node.kind, Kind::Scalar(None)) || matches!(node.as_str(), Some("null" | "~")) {
        return Ok(None);
    }
    let value = node.as_str().ok_or_else(|| {
        ConfigError::invalid_data(format!("{path} must be a context size or null"))
    })?;
    ContextSize::parse(value)
        .map(Some)
        .map_err(|error| ConfigError::invalid_data(format!("{path}: {error}")))
}

pub fn read_context_limits(fields: &Node, path: &str) -> Result<Option<ModelPresetContextLimits>> {
    let Some(limits) = fields.get("context_limits") else {
        return Ok(None);
    };
    mapping(limits, path)?;
    validate_keys(limits, path, &["default", "model_aliases"])?;
    let mut aliases = BTreeMap::new();
    if let Some(node) = limits.get("model_aliases") {
        for (key, _) in mapping(node, &format!("{path}.model_aliases"))? {
            let name = key.as_str().ok_or_else(|| {
                ConfigError::invalid_data(format!("{path}.model_aliases keys must be strings"))
            })?;
            validate_alias_name(name)?;
            if let Some(limit) =
                read_context_limit(node, name, &format!("{path}.model_aliases.{name}"))?
            {
                aliases.insert(name.to_owned(), limit);
            }
        }
    }
    Ok(Some(ModelPresetContextLimits {
        default: read_context_limit(limits, "default", &format!("{path}.default"))?,
        model_aliases: aliases,
    }))
}

pub fn write_context_limits(limits: &ModelPresetContextLimits) -> Node {
    let mut root = Node::mapping(Vec::new());
    if let Some(limit) = &limits.default {
        root.set("default", Node::scalar(limit.to_string()));
    }
    root.set(
        "model_aliases",
        Node::mapping(
            limits
                .model_aliases
                .iter()
                .map(|(name, limit)| (Node::scalar(name), Node::scalar(limit.to_string())))
                .collect(),
        ),
    );
    root
}

pub fn apply_context_limits(
    root: &mut Node,
    aliases: &mut BTreeMap<String, ModelAliasConfig>,
    limits: &ModelPresetContextLimits,
) -> Result<()> {
    for name in limits.model_aliases.keys() {
        if !aliases.contains_key(name) {
            return Err(ConfigError::invalid_data(format!(
                "model alias \"{name}\" is not defined"
            )));
        }
    }
    root.set(
        "context_limit",
        Node::scalar(
            limits
                .default
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "null".to_owned()),
        ),
    );
    ensure_mapping(root, "model_aliases")?;
    let fields = root.get_mut("model_aliases").expect("mapping inserted");
    for (name, alias) in aliases {
        alias.context_limit = limits.model_aliases.get(name).cloned();
        ensure_mapping(fields, name)?;
        fields.get_mut(name).expect("mapping inserted").set(
            "context_limit",
            Node::scalar(
                alias
                    .context_limit
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "null".to_owned()),
            ),
        );
    }
    Ok(())
}

fn ensure_mapping(parent: &mut Node, key: &str) -> Result<()> {
    if parent.get(key).is_none() {
        parent.set(key, Node::mapping(Vec::new()));
    }
    mapping(parent.get(key).expect("mapping present"), key)?;
    Ok(())
}

pub fn read_model_aliases(root: &Node) -> Result<BTreeMap<String, ModelAliasConfig>> {
    let mut result = BTreeMap::new();
    let Some(configured) = root.get("model_aliases") else {
        return Ok(result);
    };
    for (key, fields) in mapping(configured, "model_aliases")? {
        let name = key.as_str().ok_or_else(|| {
            ConfigError::invalid_data("model_aliases keys must be strings".into())
        })?;
        validate_alias_name(name)?;
        let path = format!("model_aliases.{name}");
        mapping(fields, &path)?;
        for (key, _) in mapping(fields, &path)? {
            if !matches!(
                key.as_str(),
                Some("model_string" | "usage" | "augment_system_prompt" | "icon" | "context_limit")
            ) {
                return Err(ConfigError::invalid_data(format!(
                    "{path} contains an unsupported key"
                )));
            }
        }
        let inherited: Option<&ModelAliasConfig> = result.get(name);
        let model_string = if fields.get("model_string").is_some() {
            required_scalar(fields, "model_string", &format!("{path}.model_string"))?
        } else {
            inherited
                .map(|v| v.model_string.clone())
                .unwrap_or_default()
        };
        let usage = if fields.get("usage").is_some() {
            required_scalar(fields, "usage", &format!("{path}.usage"))?
        } else {
            inherited.map(|v| v.usage.clone()).unwrap_or_default()
        };
        let augmentation = match fields.get("augment_system_prompt") {
            Some(node) => match &node.kind {
                Kind::Scalar(value) => value.clone().filter(|s| s != "null"),
                _ => {
                    return Err(ConfigError::invalid_data(format!(
                        "{path}.augment_system_prompt must be a string or null"
                    )));
                }
            },
            None => inherited.and_then(|v| v.augment_system_prompt.clone()),
        };
        if usage.is_empty() {
            return Err(ConfigError::invalid_data(format!(
                "{path}.usage must not be empty"
            )));
        }
        let icon = read_model_alias_icon(fields, name)?;
        validate_model_selector(&format!("{path}.model_string"), &model_string, true)?;
        result.insert(
            name.to_owned(),
            ModelAliasConfig {
                model_string,
                usage,
                augment_system_prompt: augmentation,
                icon,
                context_limit: read_context_limit(
                    fields,
                    "context_limit",
                    &format!("{path}.context_limit"),
                )?,
            },
        );
    }
    Ok(result)
}

fn read_model_alias_icon(fields: &Node, name: &str) -> Result<Option<ModelAliasIconConfig>> {
    let path = format!("model_aliases.{name}.icon");
    let Some(node) = fields.get("icon") else {
        return Ok(None);
    };
    if matches!(&node.kind, Kind::Scalar(value) if value.as_deref().is_none_or(|s| matches!(s, "" | "null" | "Null" | "NULL" | "~")))
    {
        return Ok(None);
    }
    let message = format!("{path} must be a glyph and color mapping, null, or empty");
    node.as_mapping()
        .ok_or(ConfigError::invalid_data(message))?;
    validate_keys(node, &path, &["glyph", "color"])?;
    let glyph = required_scalar(node, "glyph", &format!("{path}.glyph"))?;
    let color = required_scalar(node, "color", &format!("{path}.color"))?;
    if ![
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "gray",
    ]
    .contains(&color.as_str())
    {
        return Err(ConfigError::invalid_data(format!(
            "{path}.color must be a supported color"
        )));
    }
    if glyph.is_empty() {
        return Ok(None);
    }
    let visible = glyph.chars().any(|character| {
        !matches!(
            get_general_category(character),
            GeneralCategory::Control
                | GeneralCategory::Format
                | GeneralCategory::NonspacingMark
                | GeneralCategory::SpacingMark
                | GeneralCategory::EnclosingMark
                | GeneralCategory::SpaceSeparator
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
                | GeneralCategory::Surrogate
        )
    });
    if glyph.graphemes(true).count() != 1
        || !visible
        || glyph.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigError::invalid_data(format!(
            "{path}.glyph must be one visible grapheme"
        )));
    }
    Ok(Some(ModelAliasIconConfig { glyph, color }))
}

pub fn read_model_presets(root: &Node) -> Result<BTreeMap<String, ModelPresetConfig>> {
    let mut result = BTreeMap::new();
    let Some(node) = root.get("model_presets") else {
        return Ok(result);
    };
    for (key, fields) in mapping(node, "model_presets")? {
        let name = key.as_str().ok_or_else(|| {
            ConfigError::invalid_data("model_presets keys must be strings".into())
        })?;
        validate_preset_name(name)?;
        let path = format!("model_presets.{name}");
        mapping(fields, &path)?;
        validate_keys(fields, &path, &["model", "model_aliases", "context_limits"])?;
        let model = required_scalar(fields, "model", &format!("{path}.model"))?;
        validate_requested_selector(&format!("{path}.model"), &model)?;
        let alias_path = format!("{path}.model_aliases");
        let entries = fields
            .get("model_aliases")
            .and_then(Node::as_mapping)
            .ok_or_else(|| ConfigError::invalid_data(format!("{alias_path} must be a mapping")))?;
        let mut aliases = BTreeMap::new();
        for (key, value) in entries {
            let (Some(name), Some(target)) = (key.as_str(), value.as_str()) else {
                return Err(ConfigError::invalid_data(format!(
                    "{alias_path} must contain string targets"
                )));
            };
            validate_alias_name(name)?;
            validate_model_selector(&format!("{alias_path}.{name}"), target, true)?;
            aliases.insert(name.to_owned(), target.to_owned());
        }
        result.insert(
            name.to_owned(),
            ModelPresetConfig {
                model,
                model_aliases: aliases,
                context_limits: read_context_limits(fields, &format!("{path}.context_limits"))?,
            },
        );
    }
    Ok(result)
}

pub fn validate_provider_model_alias_defaults(
    provider_id: &str,
    targets: &BTreeMap<String, String>,
) -> Result<()> {
    const ALIASES: [&str; 4] = ["low_llm", "medium_llm", "high_llm", "xhigh_llm"];
    if provider_id.is_empty()
        || provider_id.trim() != provider_id
        || provider_id.contains('/')
        || provider_id.chars().any(char::is_control)
    {
        return Err(ConfigError::invalid_data(
            "provider_model_alias_defaults provider id must be a non-empty path segment".into(),
        ));
    }
    if targets.len() != ALIASES.len() || ALIASES.iter().any(|key| !targets.contains_key(*key)) {
        return Err(ConfigError::invalid_data(format!(
            "provider_model_alias_defaults.{provider_id} must define exactly low_llm, medium_llm, high_llm, and xhigh_llm"
        )));
    }
    for alias in ALIASES {
        let field = format!("provider_model_alias_defaults.{provider_id}.{alias}");
        let target = &targets[alias];
        validate_model_selector(&field, target, false)?;
        if target.split('/').next() != Some(provider_id) {
            return Err(ConfigError::invalid_data(format!(
                "{field} must select provider {provider_id}"
            )));
        }
    }
    Ok(())
}

fn read_provider_model_alias_defaults(
    root: &Node,
) -> Result<BTreeMap<String, ProviderModelAliasDefaults>> {
    let node = root
        .get("provider_model_alias_defaults")
        .and_then(Node::as_mapping)
        .ok_or_else(|| {
            ConfigError::invalid_data("provider_model_alias_defaults must be a mapping".into())
        })?;
    let mut result = BTreeMap::new();
    for (key, value) in node {
        let Some(provider_id) = key.as_str() else {
            return Err(ConfigError::invalid_data(
                "provider_model_alias_defaults must contain named mappings".into(),
            ));
        };
        let Some(entries) = value.as_mapping() else {
            return Err(ConfigError::invalid_data(
                "provider_model_alias_defaults must contain named mappings".into(),
            ));
        };
        let mut targets = BTreeMap::new();
        for (key, value) in entries {
            let Some(alias @ ("low_llm" | "medium_llm" | "high_llm" | "xhigh_llm")) = key.as_str()
            else {
                return Err(ConfigError::invalid_data(format!(
                    "provider_model_alias_defaults.{provider_id} contains an unsupported key"
                )));
            };
            let target = value.as_str().ok_or_else(|| {
                ConfigError::invalid_data(format!(
                    "provider_model_alias_defaults.{provider_id}.{alias} must be a string"
                ))
            })?;
            if targets
                .insert(alias.to_owned(), target.to_owned())
                .is_some()
            {
                return Err(ConfigError::invalid_data(format!(
                    "provider_model_alias_defaults.{provider_id}.{alias} must be defined once"
                )));
            }
        }
        validate_provider_model_alias_defaults(provider_id, &targets)?;
        result.insert(
            provider_id.to_owned(),
            ProviderModelAliasDefaults {
                provider_id: provider_id.to_owned(),
                low_model_string: targets["low_llm"].clone(),
                medium_model_string: targets["medium_llm"].clone(),
                high_model_string: targets["high_llm"].clone(),
                xhigh_model_string: targets["xhigh_llm"].clone(),
            },
        );
    }
    Ok(result)
}

fn read_model_profiles(root: &Node) -> Result<BTreeMap<String, ModelProfileConfig>> {
    if root.get("model_augment_system_prompts").is_some() {
        return Err(ConfigError::invalid_data("model_augment_system_prompts is no longer supported; move each entry to model_profiles.<selector>.system_prompt".into()));
    }
    let mut result = BTreeMap::new();
    let Some(node) = root.get("model_profiles") else {
        return Ok(result);
    };
    for (key, fields) in mapping(node, "model_profiles")? {
        let selector = key.as_str().ok_or_else(|| {
            ConfigError::invalid_data("model_profiles keys must be strings".into())
        })?;
        let field = format!("model_profiles.{selector}");
        if selector.is_empty()
            || selector.trim() != selector
            || selector
                .chars()
                .any(|c| c.is_control() && c.is_whitespace())
            || selector.split('/').any(str::is_empty)
        {
            return Err(ConfigError::invalid_data(format!(
                "{field} key must be a trimmed slash-delimited prefix without empty segments or control whitespace"
            )));
        }
        mapping(fields, &field)?;
        validate_keys(
            fields,
            &field,
            &[
                "usage",
                "system_prompt",
                "maximum_input_tokens",
                "summary_output_tokens",
            ],
        )?;
        result.insert(
            selector.to_owned(),
            ModelProfileConfig::new(
                profile_property(fields, "usage", &field)?,
                profile_property(fields, "system_prompt", &field)?,
                profile_integer(fields, "maximum_input_tokens", &field)?,
                profile_integer(fields, "summary_output_tokens", &field)?,
            ),
        );
    }
    Ok(result)
}

fn profile_null(node: &Node) -> bool {
    node.style == Style::Plain
        && matches!(&node.kind, Kind::Scalar(value) if value.as_deref().is_none_or(|s| matches!(s, "" | "null" | "Null" | "NULL" | "~")))
}

fn profile_property(fields: &Node, key: &str, path: &str) -> Result<Option<String>> {
    let Some(node) = fields.get(key) else {
        return Ok(None);
    };
    let Kind::Scalar(value) = &node.kind else {
        return Err(ConfigError::invalid_data(format!(
            "{path}.{key} must be a string or null"
        )));
    };
    Ok(if profile_null(node) {
        None
    } else {
        value.clone()
    })
}

fn profile_integer(fields: &Node, key: &str, path: &str) -> Result<Option<i32>> {
    match fields.get(key) {
        None => Ok(None),
        Some(node) if profile_null(node) => Ok(None),
        Some(_) => positive_integer(fields, key, &format!("{path}.{key}")).map(Some),
    }
}

fn optional_boolean(parent: &Node, key: &str, path: &str) -> Result<bool> {
    match parent.get(key) {
        None => Ok(false),
        Some(node) => boolean(Some(node), path),
    }
}
fn default_boolean(parent: &Node, key: &str, path: &str) -> Result<bool> {
    match parent.get(key) {
        None => Ok(true),
        Some(node) => boolean(Some(node), path),
    }
}
fn default_integer(parent: &Node, key: &str, path: &str, default: i32) -> Result<i32> {
    if parent.get(key).is_some() {
        nonnegative_integer(parent, key, path)
    } else {
        Ok(default)
    }
}

fn read_providers(root: &Node) -> Result<BTreeMap<String, ProviderConfig>> {
    let mut result = BTreeMap::new();
    let Some(entries) = root.get("providers").and_then(Node::as_mapping) else {
        return Ok(result);
    };
    for (key, item) in entries {
        let Some(id) = key.as_str() else {
            continue;
        };
        if item.as_mapping().is_none() {
            continue;
        }
        let path = format!("providers.{id}");
        result.insert(
            id.to_owned(),
            ProviderConfig {
                provider_type: scalar_or_empty(item, "type"),
                protocol: scalar_or_empty(item, "protocol"),
                base_url: scalar_or_empty(item, "base_url"),
                api_key_env: scalar_or_empty(item, "api_key_env"),
                allow_insecure_localhost: optional_boolean(
                    item,
                    "allow_insecure_localhost",
                    &format!("{path}.allow_insecure_localhost"),
                )?,
                allow_insecure_remote: optional_boolean(
                    item,
                    "allow_insecure_remote",
                    &format!("{path}.allow_insecure_remote"),
                )?,
                allow_invalid_tls_certificate: optional_boolean(
                    item,
                    "allow_invalid_tls_certificate",
                    &format!("{path}.allow_invalid_tls_certificate"),
                )?,
                disable_websocket: default_boolean(
                    item,
                    "disable_websocket",
                    &format!("{path}.disable_websocket"),
                )?,
                image_urls: default_boolean(item, "image_urls", &format!("{path}.image_urls"))?,
                stream_idle_timeout_ms: default_integer(
                    item,
                    "stream_idle_timeout_ms",
                    &format!("{path}.stream_idle_timeout_ms"),
                    300000,
                )?,
                header_timeout_ms: default_integer(
                    item,
                    "header_timeout_ms",
                    &format!("{path}.header_timeout_ms"),
                    60000,
                )?,
                header_timeout_max_retries: default_integer(
                    item,
                    "header_timeout_max_retries",
                    &format!("{path}.header_timeout_max_retries"),
                    5,
                )?,
                headers: string_map(item, "headers"),
                provider_preferences: raw_json(item, "provider_preferences")?,
                session_header: scalar_or_empty(item, "session_header"),
                models_dev_id: scalar_or_empty(item, "models_dev_id"),
                model_defaults: read_models(item, "model_defaults")?,
                models: read_models(item, "models")?,
            },
        );
    }
    Ok(result)
}

fn read_models(provider: &Node, key: &str) -> Result<BTreeMap<String, ModelConfig>> {
    let mut result = BTreeMap::new();
    let Some(entries) = provider.get(key).and_then(Node::as_mapping) else {
        return Ok(result);
    };
    for (name, item) in entries {
        let Some(id) = name.as_str() else {
            continue;
        };
        if item.as_mapping().is_none() {
            continue;
        }
        let path = format!("{key}.{id}");
        let mut flags = 0;
        for (field, flag) in [
            ("name", ModelConfigFields::NAME),
            ("context", ModelConfigFields::CONTEXT),
            ("max_output_tokens", ModelConfigFields::MAX_OUTPUT_TOKENS),
            ("max_input_tokens", ModelConfigFields::MAX_INPUT_TOKENS),
            ("input_price", ModelConfigFields::INPUT_PRICE),
            ("cached_input_price", ModelConfigFields::CACHED_INPUT_PRICE),
            ("output_price", ModelConfigFields::OUTPUT_PRICE),
            ("tools", ModelConfigFields::TOOLS),
            ("reasoning", ModelConfigFields::REASONING),
            ("output", ModelConfigFields::OUTPUT),
            ("variants", ModelConfigFields::VARIANTS),
        ] {
            if item.get(field).is_some() {
                flags |= flag;
            }
        }
        let integer = |field: &str| {
            item.get(field)
                .and_then(Node::as_str)
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(0)
        };
        let max_input_tokens = if item.get("max_input_tokens").is_some() {
            nonnegative_integer(
                item,
                "max_input_tokens",
                &format!("{path}.max_input_tokens"),
            )?
        } else {
            0
        };
        let output = item
            .get("output")
            .and_then(Node::as_sequence)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|node| {
                        if let Kind::Scalar(value) = &node.kind {
                            Some(value.clone().unwrap_or_default())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut variants = BTreeMap::new();
        if let Some(entries) = item.get("variants").and_then(Node::as_mapping) {
            for (key, value) in entries {
                if let Some(name) = key.as_str() {
                    if value.as_mapping().is_some() {
                        variants
                            .insert(name.to_owned(), scalar_or_empty(value, "reasoning_effort"));
                    }
                }
            }
        }
        result.insert(
            id.to_owned(),
            ModelConfig {
                name: scalar_or_empty(item, "name"),
                context: integer("context"),
                max_output_tokens: integer("max_output_tokens"),
                max_input_tokens,
                input_price: nonnegative_number(
                    item,
                    "input_price",
                    &format!("{path}.input_price"),
                )?,
                cached_input_price: nonnegative_number(
                    item,
                    "cached_input_price",
                    &format!("{path}.cached_input_price"),
                )?,
                output_price: nonnegative_number(
                    item,
                    "output_price",
                    &format!("{path}.output_price"),
                )?,
                tools: scalar_or_empty(item, "tools") == "true",
                reasoning: scalar_or_empty(item, "reasoning") == "true",
                output,
                variants,
                fields: ModelConfigFields(flags),
            },
        );
    }
    Ok(result)
}

fn context_limits_snapshot(limits: &ModelPresetContextLimits) -> serde_json::Value {
    let aliases: BTreeMap<String, String> = limits
        .model_aliases
        .iter()
        .map(|(name, limit)| (name.clone(), limit.to_string()))
        .collect();
    serde_json::json!({"default":limits.default.as_ref().map(ToString::to_string),"modelAliases":aliases})
}

fn number_snapshot(value: f64) -> serde_json::Value {
    serde_json::from_str(&super::opaque_json::number_string(value))
        .expect("validated finite model price")
}

fn models_snapshot(models: &BTreeMap<String, ModelConfig>) -> serde_json::Value {
    let result: serde_json::Map<String, serde_json::Value> = models.iter().map(|(name,value)| (name.clone(),serde_json::json!({
        "name":value.name,"context":value.context,"maxOutputTokens":value.max_output_tokens,"maxInputTokens":value.max_input_tokens,"inputPrice":number_snapshot(value.input_price),"cachedInputPrice":number_snapshot(value.cached_input_price),"outputPrice":number_snapshot(value.output_price),
        "tools":value.tools,"reasoning":value.reasoning,"output":value.output,"variants":value.variants,"fields":value.fields.0,
    }))).collect();
    serde_json::Value::Object(result)
}

fn nonnegative_number(parent: &Node, key: &str, path: &str) -> Result<f64> {
    let Some(node) = parent.get(key) else {
        return Ok(0.0);
    };
    let value = node
        .as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);
    value.ok_or_else(|| {
        ConfigError::invalid_data(format!("{path} must be a finite non-negative number"))
    })
}

fn string_map(parent: &Node, key: &str) -> BTreeMap<String, String> {
    parent
        .get(key)
        .and_then(Node::as_mapping)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    Some((key.as_str()?.to_owned(), value.as_str()?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn raw_json(parent: &Node, key: &str) -> Result<String> {
    match parent.get(key) {
        None => Ok(String::new()),
        Some(node) => match node.as_str() {
            Some(value) => Ok(value.to_owned()),
            None => super::yaml_json(node),
        },
    }
}
