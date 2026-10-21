use std::cmp::Ordering;
use std::collections::HashMap;

use super::{Model, ModelMetadataFields};

/// Supplements only models present in `primary`; supplemental-only entries do
/// not become catalogue members.
#[must_use]
pub fn supplement_models(primary: &[Model], supplemental: &[Model]) -> Vec<Model> {
    let mut by_id = HashMap::<&str, &Model>::new();
    for model in supplemental {
        by_id.entry(&model.id).or_insert(model);
    }
    primary
        .iter()
        .map(|model| match by_id.get(model.id.as_str()) {
            Some(fallback) => overlay(fallback, model),
            None => model.clone(),
        })
        .collect()
}

/// Builds endpoint membership with declared/default/external fallback fields.
/// Defaults vanish after a successful endpoint response omits them; declared
/// and external models stay present. `None` means no catalogue has loaded yet.
#[must_use]
pub fn merge_models(
    fetched: Option<&[Model]>,
    declared: &[Model],
    defaults: &[Model],
    external: &[Model],
) -> Vec<Model> {
    let mut configured = Vec::<Model>::new();
    let mut indexes = HashMap::<String, usize>::new();
    add(&mut configured, &mut indexes, external);
    add(&mut configured, &mut indexes, defaults);
    add(&mut configured, &mut indexes, declared);

    let source = match fetched {
        Some(models) => models.to_vec(),
        None => configured.clone(),
    };
    let mut listed = HashMap::<String, ()>::new();
    let mut result = Vec::new();
    for item in source {
        if listed.insert(item.id.clone(), ()).is_some() {
            continue;
        }
        let model = indexes
            .get(&item.id)
            .map_or(item.clone(), |index| overlay(&configured[*index], &item));
        result.push(normalize(model));
    }
    add_missing(&mut result, &mut listed, &configured, declared);
    add_missing(&mut result, &mut listed, &configured, external);
    result.sort_by(|left, right| ordinal_cmp(&left.id, &right.id));
    result
}

fn add(configured: &mut Vec<Model>, indexes: &mut HashMap<String, usize>, models: &[Model]) {
    for model in models {
        if let Some(index) = indexes.get(&model.id).copied() {
            configured[index] = overlay(&configured[index], model);
        } else {
            indexes.insert(model.id.clone(), configured.len());
            configured.push(model.clone());
        }
    }
}

fn add_missing(
    result: &mut Vec<Model>,
    listed: &mut HashMap<String, ()>,
    configured: &[Model],
    models: &[Model],
) {
    for item in models {
        if listed.insert(item.id.clone(), ()).is_none() {
            if let Some(model) = configured.iter().find(|model| model.id == item.id) {
                result.push(normalize(model.clone()));
            }
        }
    }
}

fn normalize(mut model: Model) -> Model {
    if model.name.is_empty() {
        model.name.clone_from(&model.id);
    }
    if !model.capabilities.reasoning && !model.capabilities.variants.is_empty() {
        model.capabilities.reasoning = true;
    }
    model
}

fn overlay(fallback: &Model, preferred: &Model) -> Model {
    let mut result = preferred.clone();
    if result.provider_id.is_empty() {
        result.provider_id.clone_from(&fallback.provider_id);
    }
    choose(
        &mut result.name,
        &fallback.name,
        preferred,
        fallback,
        ModelMetadataFields::NAME,
    );
    choose(
        &mut result.context_window,
        &fallback.context_window,
        preferred,
        fallback,
        ModelMetadataFields::CONTEXT_WINDOW,
    );
    choose(
        &mut result.max_output_tokens,
        &fallback.max_output_tokens,
        preferred,
        fallback,
        ModelMetadataFields::MAX_OUTPUT_TOKENS,
    );
    choose(
        &mut result.max_input_tokens,
        &fallback.max_input_tokens,
        preferred,
        fallback,
        ModelMetadataFields::MAX_INPUT_TOKENS,
    );
    choose(
        &mut result.input_price,
        &fallback.input_price,
        preferred,
        fallback,
        ModelMetadataFields::INPUT_PRICE,
    );
    choose(
        &mut result.cached_input_price,
        &fallback.cached_input_price,
        preferred,
        fallback,
        ModelMetadataFields::CACHED_INPUT_PRICE,
    );
    choose(
        &mut result.output_price,
        &fallback.output_price,
        preferred,
        fallback,
        ModelMetadataFields::OUTPUT_PRICE,
    );
    result.capabilities.tools = if prefer(preferred, fallback, ModelMetadataFields::TOOLS) {
        preferred.capabilities.tools
    } else {
        fallback.capabilities.tools
    };
    result.capabilities.reasoning = if prefer(preferred, fallback, ModelMetadataFields::REASONING) {
        preferred.capabilities.reasoning
    } else {
        fallback.capabilities.reasoning
    };
    if prefer(preferred, fallback, ModelMetadataFields::OUTPUT) {
        result
            .capabilities
            .output
            .clone_from(&preferred.capabilities.output);
    } else {
        result
            .capabilities
            .output
            .clone_from(&fallback.capabilities.output);
    }
    if prefer(preferred, fallback, ModelMetadataFields::VARIANTS) {
        result
            .capabilities
            .variants
            .clone_from(&preferred.capabilities.variants);
    } else {
        result
            .capabilities
            .variants
            .clone_from(&fallback.capabilities.variants);
    }
    result.fields = fallback.fields.union(preferred.fields);
    result
}

fn choose<T: Clone>(
    target: &mut T,
    fallback_value: &T,
    preferred: &Model,
    fallback: &Model,
    field: ModelMetadataFields,
) {
    if !prefer(preferred, fallback, field) {
        target.clone_from(fallback_value);
    }
}

fn prefer(preferred: &Model, fallback: &Model, field: ModelMetadataFields) -> bool {
    preferred.fields.contains(field) || !fallback.fields.contains(field)
}

fn ordinal_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}
