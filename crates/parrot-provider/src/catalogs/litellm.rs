use std::collections::HashMap;

use super::json_read::Value;

use super::{
    CatalogError, Model, ModelMetadataFields, json_read, merge::supplement_models, metadata,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct LiteLlmModelInfoDecoder;

impl LiteLlmModelInfoDecoder {
    #[must_use]
    pub fn needs_supplement(models: &[Model]) -> bool {
        models
            .iter()
            .any(|model| !model.fields.contains(ModelMetadataFields::SUPPLEMENTAL))
    }

    pub fn decode(provider_id: &str, json: &str) -> Result<Vec<Model>, CatalogError> {
        let root = Value::parse(json).map_err(|error| CatalogError(error.to_string()))?;
        let data = root.get("data").and_then(Value::as_array).ok_or_else(|| {
            CatalogError("provider: model info response does not contain a data array".into())
        })?;
        let mut models: Vec<Model> = Vec::new();
        let mut indexes = HashMap::<String, usize>::new();
        let mut conflicts = HashMap::<String, ModelMetadataFields>::new();

        for item in data {
            let Some(id) = json_read::string(item, "model_name").filter(|id| !id.is_empty()) else {
                continue;
            };
            let configured = item
                .get("litellm_params")
                .and_then(|v| v.get("model_info"))
                .filter(|v| v.is_object())
                .map(|v| metadata::decode(id, provider_id, v, &["max_input_tokens"], false));
            let normalized = item
                .get("model_info")
                .filter(|v| v.is_object())
                .map(|v| metadata::decode(id, provider_id, v, &["max_input_tokens"], false));
            let Some(decoded) = (match (normalized, configured) {
                (Some(primary), Some(fallback)) => {
                    Some(supplement_models(&[primary], &[fallback]).remove(0))
                }
                (Some(primary), None) => Some(primary),
                (None, Some(model)) => Some(model),
                (None, None) => None,
            }) else {
                continue;
            };
            if let Some(index) = indexes.get(id).copied() {
                let key = id.to_owned();
                let conflict = *conflicts.get(&key).unwrap_or(&ModelMetadataFields::NONE);
                let (combined, updated) = combine_deployments(&models[index], &decoded, conflict);
                models[index] = combined;
                conflicts.insert(key, updated);
            } else {
                indexes.insert(id.to_owned(), models.len());
                models.push(decoded);
            }
        }
        Ok(models)
    }
}

fn combine_deployments(
    first: &Model,
    next: &Model,
    mut conflicts: ModelMetadataFields,
) -> (Model, ModelMetadataFields) {
    let mut combined =
        supplement_models(std::slice::from_ref(first), std::slice::from_ref(next)).remove(0);
    let both = |field| first.fields.contains(field) && next.fields.contains(field);
    if both(ModelMetadataFields::CONTEXT_WINDOW) {
        combined.context_window = first.context_window.min(next.context_window);
    }
    if both(ModelMetadataFields::MAX_OUTPUT_TOKENS) {
        combined.max_output_tokens = first.max_output_tokens.min(next.max_output_tokens);
    }
    if both(ModelMetadataFields::MAX_INPUT_TOKENS) {
        combined.max_input_tokens = first.max_input_tokens.min(next.max_input_tokens);
    }

    combine_price(
        first,
        next,
        &mut combined,
        &mut conflicts,
        ModelMetadataFields::INPUT_PRICE,
        |model| model.input_price,
        |model, value| model.input_price = value,
    );
    combine_price(
        first,
        next,
        &mut combined,
        &mut conflicts,
        ModelMetadataFields::CACHED_INPUT_PRICE,
        |model| model.cached_input_price,
        |model, value| model.cached_input_price = value,
    );
    combine_price(
        first,
        next,
        &mut combined,
        &mut conflicts,
        ModelMetadataFields::OUTPUT_PRICE,
        |model| model.output_price,
        |model, value| model.output_price = value,
    );

    if both(ModelMetadataFields::TOOLS) {
        combined.capabilities.tools = first.capabilities.tools && next.capabilities.tools;
    }
    if both(ModelMetadataFields::REASONING) {
        combined.capabilities.reasoning =
            first.capabilities.reasoning && next.capabilities.reasoning;
    }
    if both(ModelMetadataFields::OUTPUT) {
        combined.capabilities.output = first
            .capabilities
            .output
            .iter()
            .filter(|value| next.capabilities.output.contains(value))
            .cloned()
            .collect();
    }
    if both(ModelMetadataFields::VARIANTS) {
        combined.capabilities.variants = first
            .capabilities
            .variants
            .iter()
            .filter(|variant| {
                next.capabilities
                    .variants
                    .iter()
                    .any(|candidate| candidate.name == variant.name)
            })
            .cloned()
            .collect();
    }
    if combined.fields.contains(ModelMetadataFields::REASONING) && !combined.capabilities.reasoning
    {
        combined.capabilities.variants.clear();
    }
    (combined, conflicts)
}

fn combine_price(
    first: &Model,
    next: &Model,
    combined: &mut Model,
    conflicts: &mut ModelMetadataFields,
    field: ModelMetadataFields,
    get: impl Fn(&Model) -> f64,
    set: impl Fn(&mut Model, f64),
) {
    if conflicts.contains(field) {
        combined.fields.remove(field);
        set(combined, 0.0);
        return;
    }
    if !first.fields.contains(field) || !next.fields.contains(field) {
        return;
    }
    if get(first) != get(next) {
        conflicts.insert(field);
        combined.fields.remove(field);
        set(combined, 0.0);
    }
}
