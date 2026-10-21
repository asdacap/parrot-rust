use super::json_read::Value;

use super::{CatalogError, Model, StandardModelError, json_read, metadata};

#[derive(Clone, Copy, Debug, Default)]
pub struct StandardModelDecoder;

fn invalid_operation_error(value: &Value) -> StandardModelError {
    let kind = match value {
        Value::Null => "Null",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::String(_) => "String",
        Value::Number(_) => "Number",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    };
    StandardModelError::InvalidOperation(CatalogError(format!(
        "The requested operation requires an element of type 'Object', but the target element has type '{kind}'."
    )))
}

impl StandardModelDecoder {
    pub fn decode(provider_id: &str, json: &str) -> Result<Vec<Model>, CatalogError> {
        Self::decode_typed(provider_id, json).map_err(StandardModelError::into_catalog_error)
    }

    pub fn decode_typed(provider_id: &str, json: &str) -> Result<Vec<Model>, StandardModelError> {
        let root = Value::parse(json)
            .map_err(|error| StandardModelError::Json(CatalogError(error.to_string())))?;
        if !root.is_object() {
            return Err(invalid_operation_error(&root));
        }
        let mut models = Vec::new();
        let mut seen = Vec::<String>::new();
        if let Some(data) = root.get("data").and_then(Value::as_array) {
            for item in data {
                if !item.is_object() {
                    return Err(invalid_operation_error(item));
                }
                let Some(id) = json_read::string(item, "id") else {
                    continue;
                };
                if id.is_empty() || seen.iter().any(|seen| seen == id) {
                    continue;
                }
                seen.push(id.to_owned());
                models.push(metadata::decode(
                    id,
                    provider_id,
                    item,
                    &["context_window", "context_length"],
                    true,
                ));
            }
        }
        if models.is_empty() {
            return Err(StandardModelError::Provider(CatalogError(
                "provider: models response contains no usable models".into(),
            )));
        }
        Ok(models)
    }
}
