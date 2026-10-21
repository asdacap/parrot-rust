use std::collections::HashSet;

use super::json_read::Value;

use super::{CatalogError, Model, ModelCapabilities, ModelMetadataFields, from_efforts, json_read};

#[derive(Clone, Copy, Debug, Default)]
pub struct KimiModelDecoder;

impl KimiModelDecoder {
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
    if let Some(name) = json_read::string(item, "display_name") {
        model.name = name.to_owned();
        fields.insert(ModelMetadataFields::NAME);
    }
    if let Some(context) = json_read::signed_i32(item, "context_length") {
        model.context_window = context;
        fields.insert(ModelMetadataFields::CONTEXT_WINDOW);
    }
    let supports_reasoning = json_read::bool_value(item, "supports_reasoning");
    if supports_reasoning.is_some() {
        fields.insert(ModelMetadataFields::REASONING);
    }
    let mut variants = Vec::new();
    if let Some(efforts) = item.get("think_efforts").filter(|value| value.is_object()) {
        if let Some(valid) = json_read::strings(efforts, "valid_efforts") {
            fields.insert(ModelMetadataFields::VARIANTS);
            if json_read::bool_value(efforts, "support").unwrap_or(false) {
                variants = from_efforts(
                    &valid,
                    json_read::string(efforts, "default_effort").unwrap_or_default(),
                );
            }
        }
    }
    model.capabilities = ModelCapabilities {
        tools: true,
        reasoning: supports_reasoning.unwrap_or(false) || !variants.is_empty(),
        output: vec!["text".to_owned()],
        variants,
    };
    model.fields = fields;
    model
}
