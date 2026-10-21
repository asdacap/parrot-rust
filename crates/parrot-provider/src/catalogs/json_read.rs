use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;

#[derive(Debug)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    String(String),
    Number(Box<RawValue>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub(crate) fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Self> {
        self.as_object()?
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub(crate) fn as_object(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub(crate) fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    pub(crate) fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        let Self::Number(raw) = self else {
            return None;
        };
        serde_json::from_str::<serde_json::Number>(raw.get())
            .ok()?
            .as_i64()
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        let Self::Number(raw) = self else {
            return None;
        };
        serde_json::from_str::<serde_json::Number>(raw.get())
            .ok()?
            .as_f64()
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let token = raw.get().trim_start();
        match token.as_bytes().first() {
            Some(b'{') => serde_json::from_str::<Object>(raw.get())
                .map(|object| Self::Object(object.0))
                .map_err(serde::de::Error::custom),
            Some(b'[') => serde_json::from_str::<Vec<Value>>(raw.get())
                .map(Self::Array)
                .map_err(serde::de::Error::custom),
            Some(b'"') => serde_json::from_str::<String>(raw.get())
                .map(Self::String)
                .map_err(serde::de::Error::custom),
            Some(b't' | b'f') => serde_json::from_str::<bool>(raw.get())
                .map(Self::Bool)
                .map_err(serde::de::Error::custom),
            Some(b'n') => Ok(Self::Null),
            Some(b'-' | b'0'..=b'9') => Ok(Self::Number(raw)),
            _ => Err(serde::de::Error::custom("invalid JSON value")),
        }
    }
}

struct Object(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Object {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = Object;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Value>()? {
                    entries.push(entry);
                }
                Ok(Object(entries))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

pub(crate) fn string<'a>(scope: &'a Value, name: &str) -> Option<&'a str> {
    scope.get(name).and_then(Value::as_str)
}

pub(crate) fn nonnegative_i32(scope: &Value, name: &str) -> Option<i32> {
    scope
        .get(name)?
        .as_i64()?
        .try_into()
        .ok()
        .filter(|value| *value >= 0)
}

pub(crate) fn signed_i32(scope: &Value, name: &str) -> Option<i32> {
    i32::try_from(scope.get(name)?.as_i64()?).ok()
}

pub(crate) fn bool_value(scope: &Value, name: &str) -> Option<bool> {
    scope.get(name)?.as_bool()
}

pub(crate) fn nonnegative_number(scope: &Value, name: &str) -> Option<f64> {
    let value = number(scope.get(name)?)?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

pub(crate) fn number(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    value
        .as_str()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

pub(crate) fn strings(scope: &Value, name: &str) -> Option<Vec<String>> {
    Some(
        scope
            .get(name)?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}
