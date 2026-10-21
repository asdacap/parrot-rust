//! Typed source content-block replay. Validate every encountered typed member
//! before applying last-wins semantics; JsonElement inputs remain opaque until
//! the later serialization phase.
use super::raw_object::RawObject;
use super::wire_json::{normalize_raw_json, quote, selected_json_string};
use serde_json::value::RawValue;

const BLOCK_FIELDS: &[&str] = &[
    "type",
    "text",
    "source",
    "id",
    "name",
    "input",
    "tool_use_id",
    "content",
    "thinking",
    "signature",
    "data",
    "cache_control",
];
const SOURCE_FIELDS: &[&str] = &["type", "media_type", "data", "url"];
const CACHE_FIELDS: &[&str] = &["type"];

#[derive(Clone, Copy)]
enum Kind {
    Block,
    Source,
    Cache,
}
impl Kind {
    fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Block => BLOCK_FIELDS,
            Self::Source => SOURCE_FIELDS,
            Self::Cache => CACHE_FIELDS,
        }
    }
}

/// Only typed deserialization failure falls back to ordinary content. Errors
/// serializing a successfully deserialized JsonElement must escape the fallback.
pub(crate) fn blocks(raw: &str) -> Result<Option<Vec<String>>, String> {
    let Ok(values) = serde_json::from_str::<Vec<Box<RawValue>>>(raw) else {
        return Ok(None);
    };
    if values
        .iter()
        .any(|value| validate(value, Kind::Block).is_err())
    {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| serialize_object(value, Kind::Block))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn validate(raw: &RawValue, kind: Kind) -> Result<(), String> {
    let input = RawObject::new(raw).ok_or("expected typed object")?;
    let mut has_type = false;
    for (raw_name, value) in &input.members {
        let name = selected_json_string(raw_name)?;
        if !kind.fields().contains(&name.as_str()) {
            continue;
        }
        has_type |= name == "type";
        if value.get() == "null" {
            continue;
        }
        match (kind, name.as_str()) {
            (Kind::Block, "input") => {} // JsonElement does not decode its strings yet.
            (Kind::Block, "source") => validate(value, Kind::Source)?,
            (Kind::Block, "cache_control") => validate(value, Kind::Cache)?,
            (Kind::Block, "content") => {
                let children: Vec<Box<RawValue>> =
                    serde_json::from_str(value.get()).map_err(|error| error.to_string())?;
                for child in children {
                    validate(&child, Kind::Block)?;
                }
            }
            _ => {
                // Validate even an earlier duplicate that will be overwritten.
                serde_json::from_str::<String>(value.get()).map_err(|error| error.to_string())?;
            }
        }
    }
    if !has_type && !matches!(kind, Kind::Cache) {
        return Err("typed object requires type".into());
    }
    Ok(())
}

fn serialize_object(raw: &RawValue, kind: Kind) -> Result<String, String> {
    let input = RawObject::new(raw).expect("validated typed object");
    let mut output = Vec::new();
    for name in kind.fields() {
        let value = input.get(name)?;
        let Some(value) = value.filter(|value| value.get() != "null") else {
            if matches!(kind, Kind::Cache) && value.is_none() {
                output.push("\"type\":\"ephemeral\"".into());
            }
            continue;
        };
        let value = match (kind, *name) {
            (Kind::Block, "input") => normalize_raw_json(value.get())?,
            (Kind::Block, "source") => serialize_object(value, Kind::Source)?,
            (Kind::Block, "cache_control") => serialize_object(value, Kind::Cache)?,
            (Kind::Block, "content") => {
                let children: Vec<Box<RawValue>> =
                    serde_json::from_str(value.get()).expect("validated content array");
                let children = children
                    .iter()
                    .map(|child| serialize_object(child, Kind::Block))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("[{}]", children.join(","))
            }
            _ => {
                quote(&serde_json::from_str::<String>(value.get()).expect("validated typed string"))
            }
        };
        output.push(format!("{}:{value}", quote(name)));
    }
    Ok(format!("{{{}}}", output.join(",")))
}
