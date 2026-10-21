use std::collections::HashSet;

use super::json_read::Value;

use super::{
    CatalogError, Model, ModelCapabilities, ModelMetadataFields, ModelVariant, json_read,
    supported_effort,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ModelsDevCatalogueDecoder;

impl ModelsDevCatalogueDecoder {
    pub fn decode(json: &str) -> Result<Vec<(String, Vec<Model>)>, CatalogError> {
        let root = Value::parse(json).map_err(|error| CatalogError(error.to_string()))?;
        let providers = root
            .as_object()
            .ok_or_else(|| CatalogError("models.dev: catalogue root must be an object".into()))?;
        let mut result = Vec::new();
        let mut seen_providers = HashSet::new();
        for (_, provider) in providers {
            let Some(provider_id) =
                json_read::string(provider, "id").filter(|value| !value.is_empty())
            else {
                continue;
            };
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            if !seen_providers.insert(provider_id.to_owned()) {
                continue;
            }
            result.push((provider_id.to_owned(), decode_models(provider_id, models)));
        }
        Ok(result)
    }
}

fn decode_models(provider_id: &str, models: &[(String, Value)]) -> Vec<Model> {
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for (_, value) in models {
        let Some(id) = json_read::string(value, "id").filter(|id| !id.is_empty()) else {
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        output.push(decode_model(id, provider_id, value));
    }
    output
}

fn decode_model(id: &str, provider_id: &str, value: &Value) -> Model {
    let mut model = Model::new(id, provider_id);
    model.name = id.to_owned();
    let mut fields = ModelMetadataFields::NONE;
    if let Some(name) = json_read::string(value, "name") {
        model.name = name.to_owned();
        fields.insert(ModelMetadataFields::NAME);
    }
    if let Some(limit) = value.get("limit").filter(|value| value.is_object()) {
        set_int(
            limit,
            "context",
            &mut model.context_window,
            &mut fields,
            ModelMetadataFields::CONTEXT_WINDOW,
        );
        set_int(
            limit,
            "input",
            &mut model.max_input_tokens,
            &mut fields,
            ModelMetadataFields::MAX_INPUT_TOKENS,
        );
        set_int(
            limit,
            "output",
            &mut model.max_output_tokens,
            &mut fields,
            ModelMetadataFields::MAX_OUTPUT_TOKENS,
        );
    }
    if let Some(cost) = value.get("cost").filter(|value| value.is_object()) {
        set_price(
            cost,
            "input",
            &mut model.input_price,
            &mut fields,
            ModelMetadataFields::INPUT_PRICE,
            1_000_000.0,
        );
        set_price(
            cost,
            "cache_read",
            &mut model.cached_input_price,
            &mut fields,
            ModelMetadataFields::CACHED_INPUT_PRICE,
            1_000_000.0,
        );
        set_price(
            cost,
            "output",
            &mut model.output_price,
            &mut fields,
            ModelMetadataFields::OUTPUT_PRICE,
            1_000_000.0,
        );
    }
    let tools = json_read::bool_value(value, "tool_call");
    let reasoning = json_read::bool_value(value, "reasoning");
    if tools.is_some() {
        fields.insert(ModelMetadataFields::TOOLS);
    }
    if reasoning.is_some() {
        fields.insert(ModelMetadataFields::REASONING);
    }
    let output = value
        .get("modalities")
        .filter(|value| value.is_object())
        .and_then(|value| json_read::strings(value, "output"));
    if output.is_some() {
        fields.insert(ModelMetadataFields::OUTPUT);
    }
    let mut efforts = Vec::<String>::new();
    let mut has_variants = false;
    if let Some(options) = value.get("reasoning_options").and_then(Value::as_array) {
        for option in options {
            if json_read::string(option, "type") != Some("effort") {
                continue;
            }
            let Some(values) = option.get("values").and_then(Value::as_array) else {
                continue;
            };
            has_variants = true;
            for item in values {
                let Some(effort) = item.as_str() else {
                    continue;
                };
                if supported_effort(effort) && !efforts.iter().any(|current| current == effort) {
                    efforts.push(effort.to_owned());
                }
            }
        }
    }
    if has_variants {
        fields.insert(ModelMetadataFields::VARIANTS);
    }
    model.capabilities = ModelCapabilities {
        tools: tools.unwrap_or(false),
        reasoning: reasoning.unwrap_or(false) || !efforts.is_empty(),
        output: output.unwrap_or_default(),
        variants: efforts
            .into_iter()
            .map(|effort| ModelVariant::new(&effort, &effort))
            .collect(),
    };
    model.fields = fields;
    model
}

fn set_int(
    value: &Value,
    key: &str,
    target: &mut i32,
    fields: &mut ModelMetadataFields,
    field: ModelMetadataFields,
) {
    if let Some(number) = json_read::nonnegative_i32(value, key) {
        *target = number;
        fields.insert(field);
    }
}

fn set_price(
    value: &Value,
    key: &str,
    target: &mut f64,
    fields: &mut ModelMetadataFields,
    field: ModelMetadataFields,
    divisor: f64,
) {
    if let Some(number) = json_read::nonnegative_number(value, key) {
        *target = number / divisor;
        fields.insert(field);
    }
}
