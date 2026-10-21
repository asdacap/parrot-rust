use super::node::{Kind, Node};
use super::{ConfigError, Result};

/// YamlJson deliberately infers scalars even when the YAML scalar was quoted.
pub fn yaml_json(node: &Node) -> Result<String> {
    let mut output = String::new();
    write_node(node, &mut output)?;
    Ok(output)
}

fn write_node(node: &Node, output: &mut String) -> Result<()> {
    match &node.kind {
        Kind::Mapping(entries) => {
            output.push('{');
            let mut first = true;
            for (key, value) in entries {
                if let Some(key) = key.as_str() {
                    if !first {
                        output.push(',');
                    }
                    first = false;
                    output.push_str(&json_string(key));
                    output.push(':');
                    write_node(value, output)?;
                }
            }
            output.push('}');
        }
        Kind::Sequence(items) => {
            output.push('[');
            for (index, value) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_node(value, output)?;
            }
            output.push(']');
        }
        Kind::Scalar(None) => output.push_str("null"),
        Kind::Scalar(Some(value)) => {
            if matches!(value.as_str(), "true" | "false") {
                output.push_str(value);
            } else if let Some(integer) =
                invariant_numeric_text(value).and_then(|text| text.parse::<i64>().ok())
            {
                output.push_str(&integer.to_string());
            } else if let Some(number) = invariant_double(value) {
                if !number.is_finite() {
                    return Err(ConfigError::invalid_data(".NET number values such as positive and negative infinity cannot be written as valid JSON. To make it work when using 'JsonSerializer', consider specifying 'JsonNumberHandling.AllowNamedFloatingPointLiterals' (see https://learn.microsoft.com/dotnet/api/system.text.json.serialization.jsonnumberhandling).".to_owned()));
                }
                output.push_str(&number_string(number));
            } else {
                output.push_str(&json_string(value));
            }
        }
    }
    Ok(())
}

// NumberStyles.Integer/Float use only U+0009..U+000D and U+0020 as
// numeric whitespace, and permit NULs only after the complete numeric input.
fn invariant_numeric_text(value: &str) -> Option<&str> {
    let text = value
        .trim_end_matches('\0')
        .trim_matches(|c| matches!(c, '\t'..='\r' | ' '));
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut digits = index - start;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        digits += index - start;
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
    }
    (index == bytes.len()).then_some(text)
}

fn invariant_double(value: &str) -> Option<f64> {
    if let Some(text) = invariant_numeric_text(value) {
        return text.parse().ok();
    }
    // .NET's special-token fallback trims Unicode whitespace, unlike its
    // numeric grammar. It accepts NaN/Infinity, not Rust's shorter "inf".
    let text = value.trim();
    let token = text.strip_prefix(['+', '-']).unwrap_or(text);
    if token.eq_ignore_ascii_case("nan") {
        Some(f64::NAN)
    } else if token.eq_ignore_ascii_case("infinity") {
        Some(if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        })
    } else {
        None
    }
}

#[cfg(test)]
#[path = "../../tests/config/opaque_numeric.rs"]
mod opaque_numeric_tests;

/// The default System.Text.Json encoder escapes non-ASCII and HTML-sensitive characters.
pub fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' if !matches!(character, '\"' | '\'' | '&' | '+' | '<' | '>' | '`') => {
                output.push(character)
            }
            _ => {
                let mut buffer = [0; 2];
                for unit in character.encode_utf16(&mut buffer) {
                    use std::fmt::Write;
                    let _ = write!(output, "\\u{unit:04X}");
                }
            }
        }
    }
    output.push('\"');
    output
}

pub fn number_string(value: f64) -> String {
    let raw = serde_json::Number::from_f64(value)
        .expect("finite JSON number")
        .to_string();
    let negative = raw.starts_with('-');
    let unsigned = raw.trim_start_matches('-');
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, 0), |(m, e)| (m, e.parse::<i32>().unwrap_or(0)));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let leading = digits.bytes().take_while(|c| *c == b'0').count();
    let significant = digits[leading..].trim_end_matches('0');
    if significant.is_empty() {
        return if negative { "-0" } else { "0" }.to_owned();
    }
    let power = decimal + exponent - leading as i32 - 1;
    let sign = if negative { "-" } else { "" };
    if !(-4..17).contains(&power) {
        let mut result = format!("{sign}{}", &significant[..1]);
        if significant.len() > 1 {
            result.push('.');
            result.push_str(&significant[1..]);
        }
        result.push('E');
        result.push(if power >= 0 { '+' } else { '-' });
        result.push_str(&format!("{:02}", power.unsigned_abs()));
        result
    } else {
        let point = power + 1;
        if point <= 0 {
            format!("{sign}0.{}{}", "0".repeat((-point) as usize), significant)
        } else if point as usize >= significant.len() {
            format!(
                "{sign}{significant}{}",
                "0".repeat(point as usize - significant.len())
            )
        } else {
            format!(
                "{sign}{}.{}",
                &significant[..point as usize],
                &significant[point as usize..]
            )
        }
    }
}
