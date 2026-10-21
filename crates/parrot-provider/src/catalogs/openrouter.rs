use std::collections::HashSet;

use super::json_read::Value;

use super::{CatalogError, Model, ModelMetadataFields, from_efforts, json_read};

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenRouterModelDecoder;

impl OpenRouterModelDecoder {
    pub fn decode(provider_id: &str, json: &str) -> Result<Vec<Model>, CatalogError> {
        let root = Value::parse(json).map_err(|error| CatalogError(error.to_string()))?;
        let mut models = Vec::new();
        let mut seen = HashSet::new();
        if let Some(data) = root.get("data").and_then(Value::as_array) {
            for item in data {
                if !item.is_object() {
                    return Err(CatalogError(
                        "provider: model entry must be an object".into(),
                    ));
                }
                let Some(id) = json_read::string(item, "id").filter(|id| !id.is_empty()) else {
                    continue;
                };
                if !seen.insert(id.to_owned()) {
                    continue;
                }
                models.push(decode_model(id, provider_id, item));
            }
        }
        if models.is_empty() {
            return Err(CatalogError(
                "provider: models response contains no usable models".into(),
            ));
        }
        Ok(models)
    }
}

fn decode_model(id: &str, provider_id: &str, item: &Value) -> Model {
    let mut model = Model::new(id, provider_id);
    model.name = id.to_owned();
    let mut fields = ModelMetadataFields::NONE;
    if let Some(name) = json_read::string(item, "name") {
        model.name = name.to_owned();
        fields.insert(ModelMetadataFields::NAME);
    }
    if let Some(context) = json_read::signed_i32(item, "context_length") {
        model.context_window = context;
        fields.insert(ModelMetadataFields::CONTEXT_WINDOW);
    }
    if let Some(provider) = item.get("top_provider") {
        if let Some(max_tokens) = json_read::signed_i32(provider, "max_completion_tokens") {
            model.max_output_tokens = max_tokens;
            fields.insert(ModelMetadataFields::MAX_OUTPUT_TOKENS);
        }
    }
    if let Some(reasoning) = item.get("reasoning").filter(|value| value.is_object()) {
        if let Some(efforts) = json_read::strings(reasoning, "supported_efforts") {
            model.capabilities.variants = from_efforts(
                &efforts,
                json_read::string(reasoning, "default_effort").unwrap_or_default(),
            );
            fields.insert(ModelMetadataFields::REASONING);
            fields.insert(ModelMetadataFields::VARIANTS);
        }
    }
    if let Some(pricing) = item.get("pricing").filter(|value| value.is_object()) {
        set_price(
            pricing,
            "prompt",
            &mut model.input_price,
            &mut fields,
            ModelMetadataFields::INPUT_PRICE,
        );
        set_price(
            pricing,
            "input_cache_read",
            &mut model.cached_input_price,
            &mut fields,
            ModelMetadataFields::CACHED_INPUT_PRICE,
        );
        set_price(
            pricing,
            "completion",
            &mut model.output_price,
            &mut fields,
            ModelMetadataFields::OUTPUT_PRICE,
        );
    }
    model.capabilities.tools = true;
    model.capabilities.reasoning = !model.capabilities.variants.is_empty();
    model.capabilities.output = vec!["text".to_owned()];
    model.fields = fields;
    model
}

fn set_price(
    value: &Value,
    key: &str,
    target: &mut f64,
    fields: &mut ModelMetadataFields,
    field: ModelMetadataFields,
) {
    if let Some(price) = json_read::nonnegative_number(value, key) {
        *target = price;
        fields.insert(field);
    }
}
