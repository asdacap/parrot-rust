//! Escaped wire JSON that preserves numeric lexemes and duplicate object properties.

use serde_json::{Value, value::RawValue};
use std::fmt::Write;

/// JavaScript-safe escaping that also escapes the HTML-sensitive `+` and
/// backtick characters.
pub(crate) fn quote(value: &str) -> String {
    escape(value, true)
}

/// JavaScript-safe escaping for the Responses wire, where `+` and backtick
/// remain literal.
pub(crate) fn source_quote(value: &str) -> String {
    escape(value, false)
}

fn escape(value: &str, html_safe: bool) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\u{8}' => result.push_str("\\b"),
            '\t' => result.push_str("\\t"),
            '\n' => result.push_str("\\n"),
            '\u{c}' => result.push_str("\\f"),
            '\r' => result.push_str("\\r"),
            '"' => result.push_str("\\u0022"),
            '<' | '>' | '&' | '\'' => push_u16(&mut result, character as u16),
            '+' | '`' if html_safe => push_u16(&mut result, character as u16),
            character if character < ' ' => push_u16(&mut result, character as u16),
            character if html_safe && (' '..='~').contains(&character) => result.push(character),
            character if !html_safe && character.is_ascii() => result.push(character),
            character => {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    push_u16(&mut result, *unit);
                }
            }
        }
    }
    result.push('"');
    result
}

fn push_u16(output: &mut String, value: u16) {
    write!(output, "\\u{value:04X}").expect("writing into a String cannot fail");
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

/// Compacts JSON with Responses escaping while preserving numeric token
/// spellings and duplicate object properties.
pub(crate) fn source_compact(raw: &str) -> Result<String, serde_json::Error> {
    let _: Box<RawValue> = serde_json::from_str(raw)?;
    let bytes = raw.as_bytes();
    let mut result = String::with_capacity(raw.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            b'"' => {
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
                let token = raw.get(start..index).ok_or_else(|| {
                    serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid JSON string boundary",
                    ))
                })?;
                let value: String = serde_json::from_str(token)?;
                result.push_str(&source_quote(&value));
            }
            byte => {
                result.push(byte as char);
                index += 1;
            }
        }
    }
    Ok(result)
}
