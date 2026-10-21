use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

struct Fields(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = Fields;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tool arguments object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(Fields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

fn fields(json: &str, allowed: &[&str]) -> Result<Fields, String> {
    if json.trim() == "null" {
        return Err("Tool arguments must be an object.".into());
    }
    let fields: Fields = serde_json::from_str(json).map_err(|error| error.to_string())?;
    if let Some((name, _)) = fields
        .0
        .iter()
        .find(|(name, _)| !allowed.contains(&name.as_str()))
    {
        return Err(format!(
            "The JSON property '{name}' is not a recognized tool argument."
        ));
    }
    Ok(fields)
}

fn required(value: Option<String>, name: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("Tool arguments require a string '{name}'."))
}

fn string(raw: &RawValue) -> Result<Option<String>, String> {
    serde_json::from_str(raw.get()).map_err(|error| error.to_string())
}

pub(super) struct ReadInput {
    pub path: String,
    pub offset: i32,
    pub limit: i32,
}

impl ReadInput {
    pub fn parse(json: &str) -> Result<Self, String> {
        let (mut path, mut offset, mut limit) = (None, None, None);
        for (name, raw) in fields(json, &["path", "offset", "limit"])?.0 {
            match name.as_str() {
                "path" => path = string(&raw)?,
                "offset" => {
                    offset = serde_json::from_str::<Option<i32>>(raw.get())
                        .map_err(|error| error.to_string())?
                }
                "limit" => {
                    limit = serde_json::from_str::<Option<i32>>(raw.get())
                        .map_err(|error| error.to_string())?
                }
                _ => unreachable!(),
            }
        }
        Ok(Self {
            path: required(path, "path")?,
            offset: offset.unwrap_or(1),
            limit: limit.unwrap_or(2000),
        })
    }
}

pub(super) struct WriteInput {
    pub path: String,
    pub content: String,
}

impl WriteInput {
    pub fn parse(json: &str) -> Result<Self, String> {
        let (mut path, mut content) = (None, None);
        for (name, raw) in fields(json, &["path", "content"])?.0 {
            match name.as_str() {
                "path" => path = string(&raw)?,
                "content" => content = string(&raw)?,
                _ => unreachable!(),
            }
        }
        let input = Self {
            path: required(path, "path")?,
            content: required(content, "content")?,
        };
        Ok(input)
    }
}

pub(super) struct EditInput {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

impl EditInput {
    pub fn parse(json: &str) -> Result<Self, String> {
        let (mut path, mut old_string, mut new_string, mut replace_all) = (None, None, None, false);
        for (name, raw) in fields(json, &["path", "old_string", "new_string", "replace_all"])?.0 {
            match name.as_str() {
                "path" => path = string(&raw)?,
                "old_string" => old_string = string(&raw)?,
                "new_string" => new_string = string(&raw)?,
                "replace_all" => {
                    replace_all =
                        serde_json::from_str(raw.get()).map_err(|error| error.to_string())?
                }
                _ => unreachable!(),
            }
        }
        let input = Self {
            path: required(path, "path")?,
            old_string: required(old_string, "old_string")?,
            new_string: required(new_string, "new_string")?,
            replace_all,
        };
        Ok(input)
    }
}

pub(super) fn image_path(json: &str) -> Result<String, String> {
    let mut path = None;
    for (_, raw) in fields(json, &["path"])?.0 {
        path = string(&raw)?;
    }
    required(path, "path")
}

pub(super) struct GlobInput {
    pub pattern: String,
    pub path: String,
}

impl GlobInput {
    pub fn parse(json: &str) -> Result<Self, String> {
        let (mut pattern, mut path) = (None, None);
        for (name, raw) in fields(json, &["pattern", "path"])?.0 {
            match name.as_str() {
                "pattern" => pattern = string(&raw)?,
                "path" => path = string(&raw)?,
                _ => unreachable!(),
            }
        }
        Ok(Self {
            pattern: required(pattern, "pattern")?,
            path: path.unwrap_or_default(),
        })
    }
}
