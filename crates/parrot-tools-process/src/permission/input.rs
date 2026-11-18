use super::{Failure, FailureKind};
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;

pub(super) fn object(json: &str, allowed: &[&str]) -> Result<Map<String, Value>, Failure> {
    if json.trim() == "null" {
        return Err(format("Tool arguments must be an object."));
    }
    struct Fields<'a>(&'a [&'a str]);
    impl<'de> DeserializeSeed<'de> for Fields<'_> {
        type Value = Map<String, Value>;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Fields<'_> {
        type Value = Map<String, Value>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a tool argument object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut fields = Map::new();
            while let Some(name) = map.next_key::<String>()? {
                if !self.0.contains(&name.as_str()) {
                    return Err(serde::de::Error::custom(format!(
                        "Unknown tool argument '{name}'."
                    )));
                }
                let value: Value = map.next_value()?;
                let valid = if name == "paths" || name == "referenced_image_paths" {
                    value.is_null()
                        || value.as_array().is_some_and(|items| {
                            items.iter().all(|item| item.is_null() || item.is_string())
                        })
                } else {
                    value.is_null() || value.is_string()
                };
                if !valid {
                    return Err(serde::de::Error::custom(format!(
                        "Invalid type for tool argument '{name}'."
                    )));
                }
                // Source deserialization validates each occurrence, then retains
                // the last value. An earlier wrong-typed duplicate cannot hide.
                fields.insert(name, value);
            }
            Ok(fields)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let object = Fields(allowed)
        .deserialize(&mut deserializer)
        .map_err(|error| Failure::new(FailureKind::Json, error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| Failure::new(FailureKind::Json, error.to_string()))?;
    Ok(object)
}

pub(super) fn optional_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, Failure> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Failure::new(
            FailureKind::Json,
            format!("Tool argument '{name}' must be a string."),
        )),
    }
}

pub(super) fn required_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, Failure> {
    optional_string(object, name)?
        .ok_or_else(|| format(format!("Tool arguments require a string '{name}'.")))
}

pub(super) fn format(message: impl Into<String>) -> Failure {
    Failure::new(FailureKind::Format, message)
}
