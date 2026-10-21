use serde_json::value::RawValue;
use std::io;

/// Compact JSON with System.Text.Json's default JavaScript-safe escaping while
/// preserving numeric token spellings and duplicate object properties.
pub(super) fn source_compact(raw: &str) -> Result<String, serde_json::Error> {
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
                    serde_json::Error::io(io::Error::new(
                        io::ErrorKind::InvalidData,
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

pub(super) fn source_quote(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\u0022"),
            '\\' => result.push_str("\\\\"),
            '\u{0008}' => result.push_str("\\b"),
            '\u{0009}' => result.push_str("\\t"),
            '\u{000A}' => result.push_str("\\n"),
            '\u{000C}' => result.push_str("\\f"),
            '\u{000D}' => result.push_str("\\r"),
            '<' => result.push_str("\\u003C"),
            '>' => result.push_str("\\u003E"),
            '&' => result.push_str("\\u0026"),
            '\'' => result.push_str("\\u0027"),
            character if character < ' ' => push_u16(&mut result, character as u16),
            character if character.is_ascii() => result.push(character),
            character => {
                let mut units = [0; 2];
                for unit in character.encode_utf16(&mut units).iter() {
                    push_u16(&mut result, *unit);
                }
            }
        }
    }
    result.push('"');
    result
}

fn push_u16(output: &mut String, value: u16) {
    use std::fmt::Write;
    write!(output, "\\u{value:04X}").expect("writing into a String cannot fail");
}
