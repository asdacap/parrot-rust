//! Source URI spelling at image-operation boundaries, after authority policy.
use crate::{http_stream::Endpoint, sessions::Error};
use std::fmt::Write;

/// Preserve authority/TLS identity while applying the source URI constructor's
/// path/query/fragment escaping. Signing and dispatch consume this same endpoint.
pub(crate) fn normalize_endpoint(endpoint: &Endpoint) -> Result<Endpoint, Error> {
    let url = endpoint.as_url();
    let mut value = endpoint.authority().to_owned();
    value.push_str(&component(url.path()));
    if let Some(query) = url.query() {
        value.push('?');
        value.push_str(&component(query));
    }
    if let Some(fragment) = url.fragment() {
        value.push('#');
        value.push_str(&component(fragment));
    }
    Endpoint::parse(&value).map_err(|error| Error::Provider(error.to_string()))
}

fn component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut offset = 0;
    while offset < value.len() {
        if value.as_bytes()[offset] != b'%' {
            let character = value[offset..].chars().next().unwrap();
            output.push(character);
            offset += character.len_utf8();
            continue;
        }
        let start = offset;
        let mut octets = Vec::new();
        while value.as_bytes().get(offset) == Some(&b'%') && offset + 2 < value.len() {
            let pair = &value.as_bytes()[offset + 1..offset + 3];
            let (Some(a), Some(b)) = (
                (pair[0] as char).to_digit(16),
                (pair[1] as char).to_digit(16),
            ) else {
                break;
            };
            octets.push((a * 16 + b) as u8);
            offset += 3;
        }
        if octets.is_empty() {
            output.push_str("%25");
            offset += 1;
            continue;
        }
        let mut consumed = 0;
        while consumed < octets.len() {
            match std::str::from_utf8(&octets[consumed..]) {
                Ok(text) => {
                    append_valid(&mut output, text, &value[start + consumed * 3..offset]);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    append_valid(
                        &mut output,
                        std::str::from_utf8(&octets[consumed..consumed + valid]).unwrap(),
                        &value[start + consumed * 3..start + (consumed + valid) * 3],
                    );
                    consumed += valid;
                    let invalid = error.error_len().unwrap_or(octets.len() - consumed);
                    output.push_str(&value[start + consumed * 3..start + (consumed + invalid) * 3]);
                    consumed += invalid;
                }
            }
        }
    }
    output
}

fn append_valid(output: &mut String, text: &str, original: &str) {
    for (index, character) in text.char_indices() {
        if character.is_ascii_alphanumeric() || "-._~".contains(character) {
            output.push(character);
        } else if character.is_ascii() {
            // Reserved ASCII escapes retain their original hex casing.
            output.push_str(&original[index * 3..index * 3 + 3]);
        } else {
            let mut buffer = [0; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                write!(output, "%{byte:02X}").unwrap();
            }
        }
    }
}
