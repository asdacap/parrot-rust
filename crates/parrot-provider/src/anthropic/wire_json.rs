//! Anthropic's source serializer uses the default .NET JSON encoder. Preserve
//! numeric lexemes and duplicate object properties when replaying JsonElement.
use serde_json::{Value, value::RawValue};
use std::fmt::Write;

pub(crate) fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            ' '..='~' if !matches!(character, '"' | '\'' | '<' | '>' | '&' | '+' | '`') => {
                result.push(character)
            }
            _ => {
                for unit in character.encode_utf16(&mut [0; 2]) {
                    write!(result, "\\u{unit:04X}").unwrap();
                }
            }
        }
    }
    result.push('"');
    result
}

pub(crate) fn serialize(value: &Value) -> String {
    normalize_raw_json(&serde_json::to_string(value).unwrap()).expect("valid serialized value")
}

/// A lexical serializer over an already validated JSON document: unlike a
/// floating-point value tree, this retains arbitrarily large JSON numbers.
pub(crate) fn normalize_raw_json(raw: &str) -> Result<String, String> {
    serde_json::from_str::<Box<RawValue>>(raw).map_err(|error| format_json_error(raw, &error))?;
    let bytes = raw.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if bytes[index] == b'"' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => {
                        index += 1;
                        break;
                    }
                    _ => index += 1,
                }
            }
            let value = selected_json_string(&raw[start..index])
                .map_err(|_| "The object or value could not be serialized. Path: $.".to_owned())?;
            output.push_str(&quote(&value));
        } else {
            // JSON structural characters, literals and numeric tokens are ASCII.
            output.push(char::from(bytes[index]));
            index += 1;
        }
    }
    Ok(output)
}

/// Preserve the source's property-name diagnostic using the actual parser
/// location/token. Other native parser diagnostics retain their original text.
pub(crate) fn format_json_error(raw: &str, error: &serde_json::Error) -> String {
    if error.to_string().starts_with("key must be a string") {
        let line = error.line().saturating_sub(1);
        let column = error.column().saturating_sub(1);
        if let Some(token) = raw
            .split('\n')
            .nth(line)
            .and_then(|line| line.as_bytes().get(column))
        {
            return format!(
                "'{}' is an invalid start of a property name. Expected a '\"'. LineNumber: {line} | BytePositionInLine: {column}.",
                char::from(*token)
            );
        }
    }
    error.to_string()
}

pub(crate) fn selected_json_string(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut index = 1;
    while index + 1 < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        if bytes[index + 1] != b'u' {
            index += 2;
            continue;
        }
        let unit =
            u16::from_str_radix(&raw[index + 2..index + 6], 16).expect("validated JSON escape");
        index += 6;
        if (0xd800..=0xdbff).contains(&unit) {
            let low = bytes
                .get(index..index + 2)
                .filter(|escape| *escape == b"\\u")
                .and_then(|_| raw.get(index + 2..index + 6))
                .and_then(|digits| u16::from_str_radix(digits, 16).ok());
            if !low.is_some_and(|unit| (0xdc00..=0xdfff).contains(&unit)) {
                return Err(
                    "Cannot read incomplete UTF-16 JSON text as string with missing low surrogate."
                        .into(),
                );
            }
            index += 6;
        } else if (0xdc00..=0xdfff).contains(&unit) {
            return Err(format!(
                "Cannot read invalid UTF-16 JSON text as string. Invalid surrogate value: '0x{unit:04X}'."
            ));
        }
    }
    Ok(serde_json::from_str(raw).expect("validated JSON string with paired surrogates"))
}
