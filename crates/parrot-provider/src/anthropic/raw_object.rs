//! Ordered members of an already validated JSON object. Property names remain
//! raw until a source-style lookup actually visits them.
use serde_json::value::RawValue;

use super::wire_json::selected_json_string;

pub(crate) struct RawObject<'a> {
    pub(crate) members: Vec<(&'a str, &'a RawValue)>,
}

impl<'a> RawObject<'a> {
    pub(crate) fn new(raw: &'a RawValue) -> Option<Self> {
        let text = raw.get();
        if !text.starts_with('{') {
            return None;
        }
        let bytes = text.as_bytes();
        let mut index = 1;
        let mut members = Vec::new();
        loop {
            while bytes[index].is_ascii_whitespace() || bytes[index] == b',' {
                index += 1;
            }
            if bytes[index] == b'}' {
                break;
            }
            let start = index;
            index += 1;
            while bytes[index] != b'"' {
                index += if bytes[index] == b'\\' { 2 } else { 1 };
            }
            index += 1;
            let name = &text[start..index];
            while bytes[index].is_ascii_whitespace() || bytes[index] == b':' {
                index += 1;
            }
            let mut stream =
                serde_json::Deserializer::from_str(&text[index..]).into_iter::<&RawValue>();
            let value = stream
                .next()
                .expect("validated object value")
                .expect("validated JSON");
            index += stream.byte_offset();
            members.push((name, value));
        }
        Some(Self { members })
    }

    pub(crate) fn get(&self, name: &str) -> Result<Option<&'a RawValue>, String> {
        for (raw_name, value) in self.members.iter().rev() {
            let raw = &raw_name.as_bytes()[1..raw_name.len() - 1];
            if raw.len() < name.len() {
                continue;
            }
            // JsonElement compares the unescaped prefix before decoding the
            // suffix. A mismatch can leave malformed escapes entirely unused.
            let Some(escape) = raw.iter().position(|byte| *byte == b'\\') else {
                if raw == name.as_bytes() {
                    return Ok(Some(value));
                }
                continue;
            };
            if name.as_bytes().get(..escape) != Some(&raw[..escape]) {
                continue;
            }
            if selected_json_string(raw_name)? == name {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}
