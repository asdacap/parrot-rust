use crate::{CompactionRequest, ContextLimit, Error, ErrorCode};
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use std::fmt;

struct CompactionArguments;

impl<'de> DeserializeSeed<'de> for CompactionArguments {
    type Value = Option<String>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for CompactionArguments {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a tool argument object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut target = None;
        while let Some(name) = map.next_key::<String>()? {
            if name != "target_context_size" {
                return Err(serde::de::Error::custom(format!(
                    "Unknown tool argument '{name}'."
                )));
            }
            target = map.next_value::<Option<String>>()?;
        }
        Ok(target)
    }
}

impl CompactionRequest {
    pub fn parse(arguments_json: &str) -> Result<Self, Error> {
        let argument_error = |message: String| Error {
            code: ErrorCode::Argument,
            ..Error::failed(message)
        };
        if arguments_json.trim() == "null" {
            return Err(argument_error("Tool arguments must be an object.".into()));
        }
        let mut deserializer = serde_json::Deserializer::from_str(arguments_json);
        let target = CompactionArguments
            .deserialize(&mut deserializer)
            .map_err(|error| argument_error(error.to_string()))?;
        deserializer
            .end()
            .map_err(|error| argument_error(error.to_string()))?;
        let target = target
            .map(|text| {
                if text.is_empty() {
                    return Err(argument_error(
                        "target_context_size must not be empty.".into(),
                    ));
                }
                ContextLimit::parse(&text)
            })
            .transpose()?;
        Ok(Self { target })
    }
}
