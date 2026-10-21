use super::json_read::Value;

use super::{
    Model, ModelCapabilities, ModelMetadataFields, from_efforts, json_read, supported_effort,
};

pub(crate) fn decode(
    id: &str,
    provider_id: &str,
    metadata: &Value,
    context_fields: &[&str],
    read_name: bool,
) -> Model {
    let mut model = Model::new(id, provider_id);
    model.name = id.to_owned();
    let mut fields = ModelMetadataFields::NONE;
    if read_name {
        if let Some(name) = json_read::string(metadata, "name") {
            model.name = name.to_owned();
            fields.insert(ModelMetadataFields::NAME);
        }
    }
    for field in context_fields {
        if let Some(context) = json_read::nonnegative_i32(metadata, field) {
            model.context_window = context;
            fields.insert(ModelMetadataFields::CONTEXT_WINDOW);
            break;
        }
    }
    set_int(
        metadata,
        "max_output_tokens",
        &mut model.max_output_tokens,
        &mut fields,
        ModelMetadataFields::MAX_OUTPUT_TOKENS,
    );
    set_int(
        metadata,
        "max_input_tokens",
        &mut model.max_input_tokens,
        &mut fields,
        ModelMetadataFields::MAX_INPUT_TOKENS,
    );
    set_price(
        metadata,
        "input_cost_per_token",
        &mut model.input_price,
        &mut fields,
        ModelMetadataFields::INPUT_PRICE,
    );
    set_price(
        metadata,
        "cache_read_input_token_cost",
        &mut model.cached_input_price,
        &mut fields,
        ModelMetadataFields::CACHED_INPUT_PRICE,
    );
    set_price(
        metadata,
        "output_cost_per_token",
        &mut model.output_price,
        &mut fields,
        ModelMetadataFields::OUTPUT_PRICE,
    );

    let tools = json_read::bool_value(metadata, "supports_function_calling");
    let reasoning = json_read::bool_value(metadata, "supports_reasoning");
    if tools.is_some() {
        fields.insert(ModelMetadataFields::TOOLS);
    }
    if reasoning.is_some() {
        fields.insert(ModelMetadataFields::REASONING);
    }

    let output = json_read::strings(metadata, "supported_output_modalities");
    if output.is_some() {
        fields.insert(ModelMetadataFields::OUTPUT);
    }

    let efforts = json_read::strings(metadata, "supported_reasoning_efforts")
        .or_else(|| json_read::strings(metadata, "reasoning_effort_levels"));
    if efforts.is_some() || (reasoning == Some(false)) {
        fields.insert(ModelMetadataFields::VARIANTS);
    }
    let variants = efforts
        .unwrap_or_default()
        .into_iter()
        .filter(|value| supported_effort(value))
        .fold(Vec::new(), |mut output, value| {
            if !output.contains(&value) {
                output.push(value);
            }
            output
        });
    let variants = from_efforts(
        &variants,
        json_read::string(metadata, "default_reasoning_effort").unwrap_or_default(),
    );

    model.capabilities = ModelCapabilities {
        tools: tools.unwrap_or(true),
        reasoning: reasoning == Some(true) || !variants.is_empty(),
        output: output.unwrap_or_else(|| vec!["text".to_owned()]),
        variants,
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
) {
    if let Some(number) = json_read::nonnegative_number(value, key) {
        *target = number;
        fields.insert(field);
    }
}
