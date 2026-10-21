use super::{Error, Result};
use crate::conversation::AgentHistoryEntry;
use serde::Serialize;
use serde_json::ser::{CharEscape, CompactFormatter, Formatter, PrettyFormatter};
use serde_json::{Map, Number, Value};
use std::io;

#[path = "json_decode.rs"]
mod decode;
pub use decode::deserialize_entry;

pub fn serialize_entry(entry: &AgentHistoryEntry) -> Result<Vec<u8>> {
    let mut object = Map::new();
    match entry {
        AgentHistoryEntry::Message {
            sequence,
            conversation_sequence,
            origin,
            role,
            parts,
            tool_calls,
            tool_call_id,
        } => {
            string(&mut object, "type", "message");
            integer(&mut object, "conversation_sequence", *conversation_sequence);
            string(&mut object, "origin", origin);
            string(&mut object, "role", role);
            object.insert(
                "parts".into(),
                Value::Array(
                    parts
                        .iter()
                        .map(|part| {
                            let mut value = Map::new();
                            match part {
                                crate::conversation::ConversationPart::Text(text) => {
                                    string(&mut value, "kind", "text");
                                    string(&mut value, "text", text);
                                    string(&mut value, "artifact_id", "");
                                    string(&mut value, "media_type", "");
                                    string(&mut value, "display_name", "");
                                }
                                crate::conversation::ConversationPart::ImageArtifact {
                                    artifact_id,
                                    media_type,
                                    display_name,
                                } => {
                                    string(&mut value, "kind", "image_artifact");
                                    string(&mut value, "text", "");
                                    string(&mut value, "artifact_id", artifact_id);
                                    string(&mut value, "media_type", media_type);
                                    string(&mut value, "display_name", display_name);
                                }
                            }
                            Value::Object(value)
                        })
                        .collect(),
                ),
            );
            object.insert(
                "tool_calls".into(),
                Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            let mut value = Map::new();
                            string(&mut value, "id", &call.id);
                            string(&mut value, "name", &call.name);
                            string(&mut value, "arguments_json", &call.arguments_json);
                            Value::Object(value)
                        })
                        .collect(),
                ),
            );
            string(&mut object, "tool_call_id", tool_call_id);
            integer(&mut object, "sequence", *sequence);
        }
        AgentHistoryEntry::Compaction {
            sequence,
            summary,
            watermark,
        } => {
            string(&mut object, "type", "compaction");
            string(&mut object, "summary", summary);
            integer(&mut object, "watermark", *watermark);
            integer(&mut object, "sequence", *sequence);
        }
        AgentHistoryEntry::Request {
            sequence,
            event_sequence,
            request_id,
            provider,
            model,
            effort,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            input_cost,
            output_cost,
            tool_calls,
        } => {
            string(&mut object, "type", "request");
            integer(&mut object, "event_sequence", *event_sequence);
            string(&mut object, "request_id", request_id);
            string(&mut object, "provider", provider);
            string(&mut object, "model", model);
            object.insert(
                "effort".into(),
                effort.clone().map(Value::String).unwrap_or(Value::Null),
            );
            integer(&mut object, "input_tokens", *input_tokens);
            integer(&mut object, "cached_input_tokens", *cached_input_tokens);
            integer(&mut object, "output_tokens", *output_tokens);
            number(&mut object, "input_cost", *input_cost)?;
            number(&mut object, "output_cost", *output_cost)?;
            integer(&mut object, "tool_calls", *tool_calls);
            number(&mut object, "total_cost", input_cost + output_cost)?;
            integer(&mut object, "sequence", *sequence);
        }
    }
    let mut bytes = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut bytes, DotNetJsonFormatter(CompactFormatter));
    Value::Object(object)
        .serialize(&mut serializer)
        .map_err(|error| Error::invalid_data(format!("serialize history entry: {error}")))?;
    Ok(bytes)
}

pub(crate) fn serialize_metadata(metadata: &super::SessionMeta) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut bytes,
        DotNetJsonFormatter(PrettyFormatter::new()),
    );
    metadata
        .serialize(&mut serializer)
        .map_err(|error| Error::invalid_data(format!("serialize session metadata: {error}")))?;
    Ok(bytes)
}

struct DotNetJsonFormatter<F>(F);

macro_rules! delegate_layout {
    ($method:ident $(, $argument:ident: $kind:ty)*) => {
        fn $method<W>(&mut self, writer: &mut W $(, $argument: $kind)*) -> io::Result<()>
        where
            W: ?Sized + io::Write,
        {
            self.0.$method(writer $(, $argument)*)
        }
    };
}

impl<F: Formatter> Formatter for DotNetJsonFormatter<F> {
    delegate_layout!(begin_array);
    delegate_layout!(end_array);
    delegate_layout!(begin_array_value, first: bool);
    delegate_layout!(end_array_value);
    delegate_layout!(begin_object);
    delegate_layout!(end_object);
    delegate_layout!(begin_object_key, first: bool);
    delegate_layout!(end_object_key);
    delegate_layout!(begin_object_value);
    delegate_layout!(end_object_value);

    fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        // Both runtimes use shortest round-trip digits, but .NET's general
        // notation keeps integral doubles without '.0' and uses signed E+NN.
        let rendered = value.to_string();
        let unsigned = rendered.trim_start_matches('-');
        let decimal = unsigned.find('.').unwrap_or(unsigned.len());
        let digits: String = unsigned
            .chars()
            .filter(|character| *character != '.')
            .collect();
        let Some(first) = digits.find(|character| character != '0') else {
            return writer.write_all(if value.is_sign_negative() {
                b"-0"
            } else {
                b"0"
            });
        };
        let exponent = decimal as i32 - first as i32 - 1;
        if !(-4..17).contains(&exponent) {
            if value.is_sign_negative() {
                writer.write_all(b"-")?;
            }
            let significant = digits[first..].trim_end_matches('0');
            writer.write_all(&significant.as_bytes()[..1])?;
            if significant.len() > 1 {
                writer.write_all(b".")?;
                writer.write_all(&significant.as_bytes()[1..])?;
            }
            write!(writer, "E{exponent:+03}")
        } else {
            writer.write_all(rendered.as_bytes())
        }
    }

    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        for character in fragment.chars() {
            if (' '..='~').contains(&character)
                && !matches!(character, '<' | '>' | '&' | '\'' | '+' | '`')
            {
                let mut buffer = [0; 4];
                writer.write_all(character.encode_utf8(&mut buffer).as_bytes())?;
            } else {
                for code_unit in character.encode_utf16(&mut [0; 2]).iter().copied() {
                    write_unicode_escape(writer, code_unit)?;
                }
            }
        }
        Ok(())
    }

    fn write_char_escape<W>(&mut self, writer: &mut W, escape: CharEscape) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        match escape {
            CharEscape::Quote => write_unicode_escape(writer, b'"' as u16),
            CharEscape::AsciiControl(character) => write_unicode_escape(writer, character as u16),
            _ => CompactFormatter.write_char_escape(writer, escape),
        }
    }
}

fn write_unicode_escape<W>(writer: &mut W, value: u16) -> io::Result<()>
where
    W: ?Sized + io::Write,
{
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    writer.write_all(&[
        b'\\',
        b'u',
        HEX[((value >> 12) & 0xf) as usize],
        HEX[((value >> 8) & 0xf) as usize],
        HEX[((value >> 4) & 0xf) as usize],
        HEX[(value & 0xf) as usize],
    ])
}

fn string(object: &mut Map<String, Value>, key: &str, value: &str) {
    object.insert(key.to_owned(), Value::String(value.to_owned()));
}

fn integer(object: &mut Map<String, Value>, key: &str, value: i64) {
    object.insert(key.to_owned(), Value::Number(Number::from(value)));
}

fn number(object: &mut Map<String, Value>, key: &str, value: f64) -> Result<()> {
    let value = Number::from_f64(value).ok_or_else(|| {
        Error::invalid_data(format!("history field {key} is not a finite JSON number"))
    })?;
    object.insert(key.to_owned(), Value::Number(value));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{AgentHistoryEntry, ConversationPart, ToolCall};

    const BOUNDARY_BYTES: &str =
        include_str!("../../tests/history_files/fixtures/escaping.txt");

    #[test]
    fn metadata_and_history_escape_boundaries_match_system_text_json_bytes() {
        let input: String = (0..128)
            .map(|value| char::from_u32(value).unwrap())
            .chain("é雪😀\u{85}\u{2028}\u{2029}".chars())
            .collect();
        let escaped = BOUNDARY_BYTES.lines().next().unwrap();
        let entry = AgentHistoryEntry::Compaction {
            sequence: 7,
            summary: input.clone(),
            watermark: 4,
        };
        assert_eq!(
            serialize_entry(&entry).unwrap(),
            format!(
                "{{\"type\":\"compaction\",\"summary\":{escaped},\"watermark\":4,\"sequence\":7}}"
            )
            .as_bytes()
        );
        let metadata = super::super::SessionMeta::new("listed", "/workspace", input, "model");
        let bytes = serialize_metadata(&metadata).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(&format!("  \"ProviderId\": {escaped},\n")));
        assert_eq!(
            serde_json::from_str::<super::super::SessionMeta>(&text).unwrap(),
            metadata
        );
    }

    #[test]
    fn request_double_rendering_matches_system_text_json_boundaries() {
        let values = [
            0.0,
            -0.0,
            1.0,
            2.0,
            3.0,
            0.125,
            0.0001,
            0.00001,
            1e15,
            1e16,
            1e17,
            1e20,
            1e-7,
            1.2345678901234567e20,
            f64::from_bits(1),
            f64::MAX,
        ];
        for (value, expected) in values.into_iter().zip(BOUNDARY_BYTES.lines().skip(1)) {
            let mut actual = Vec::new();
            DotNetJsonFormatter(CompactFormatter)
                .write_f64(&mut actual, value)
                .unwrap();
            assert_eq!(actual, expected.as_bytes(), "value {value}");
        }
        let entry = AgentHistoryEntry::Request {
            sequence: 3,
            event_sequence: 15,
            request_id: "request".into(),
            provider: "provider".into(),
            model: "model".into(),
            effort: None,
            input_tokens: 100,
            cached_input_tokens: 25,
            output_tokens: 50,
            input_cost: 1.0,
            output_cost: 2.0,
            tool_calls: 2,
        };
        assert_eq!(
            serialize_entry(&entry).unwrap(),
            br#"{"type":"request","event_sequence":15,"request_id":"request","provider":"provider","model":"model","effort":null,"input_tokens":100,"cached_input_tokens":25,"output_tokens":50,"input_cost":1,"output_cost":2,"tool_calls":2,"total_cost":3,"sequence":3}"#
        );
    }

    #[test]
    fn discriminated_history_jsonl_matches_the_independent_dotnet_payload() {
        let entries = [
            AgentHistoryEntry::Compaction {
                sequence: 7,
                summary: "summary\n雪".into(),
                watermark: 4,
            },
            AgentHistoryEntry::Message {
                sequence: 2,
                conversation_sequence: 9,
                origin: "model".into(),
                role: "assistant".into(),
                parts: vec![ConversationPart::Text("hello".into())],
                tool_calls: vec![ToolCall {
                    id: "call".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                }],
                tool_call_id: String::new(),
            },
            AgentHistoryEntry::Request {
                sequence: 3,
                event_sequence: 15,
                request_id: "request-one".into(),
                provider: "provider".into(),
                model: "model".into(),
                effort: None,
                input_tokens: 100,
                cached_input_tokens: 25,
                output_tokens: 50,
                input_cost: 0.125,
                output_cost: 0.25,
                tool_calls: 2,
            },
        ];
        let evidence: Value = serde_json::from_str(include_str!("../../tests/history_files/fixtures/format-cases.json"))
        .unwrap();
        let oracle = evidence;
        let expected = oracle["observations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|observation| observation["id"] == "history.all-discriminators")
            .unwrap()["text"]
            .as_str()
            .unwrap();
        let mut actual = Vec::new();
        for entry in &entries {
            actual.extend_from_slice(&serialize_entry(entry).unwrap());
            actual.push(b'\n');
        }

        assert_eq!(actual, expected.as_bytes());
    }
}
