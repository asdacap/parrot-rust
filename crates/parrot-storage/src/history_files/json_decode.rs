use super::super::{Error, Result};
use crate::conversation::{AgentHistoryEntry, ConversationPart, ToolCall};
use serde::Deserialize;
use serde_json::Value;

/// Decode one source-format history record. This is not a repository restore path:
/// SQLite remains authoritative. Null reference carriers and part shapes that the
/// shared Rust history model cannot preserve are rejected rather than normalized.
pub fn deserialize_entry(bytes: &[u8]) -> Result<AgentHistoryEntry> {
    let entry: HistoryEntry = serde_json::from_slice(bytes).map_err(decode_error)?;
    match entry {
        HistoryEntry::Compaction(entry) => Ok(AgentHistoryEntry::Compaction {
            sequence: entry.sequence,
            summary: entry.summary,
            watermark: entry.watermark,
        }),
        HistoryEntry::Message(entry) => Ok(AgentHistoryEntry::Message {
            sequence: entry.sequence,
            conversation_sequence: entry.conversation_sequence,
            origin: entry.origin,
            role: entry.role,
            parts: entry
                .parts
                .into_iter()
                .map(Part::into_part)
                .collect::<Result<Vec<_>>>()?,
            tool_calls: entry
                .tool_calls
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments_json: call.arguments_json,
                })
                .collect(),
            tool_call_id: entry.tool_call_id,
        }),
        HistoryEntry::Request(entry) => Ok(AgentHistoryEntry::Request {
            sequence: entry.sequence,
            event_sequence: entry.event_sequence,
            request_id: entry.request_id,
            provider: entry.provider,
            model: entry.model,
            effort: entry.effort,
            input_tokens: entry.input_tokens,
            cached_input_tokens: entry.cached_input_tokens,
            output_tokens: entry.output_tokens,
            input_cost: entry.input_cost,
            output_cost: entry.output_cost,
            tool_calls: entry.tool_calls,
        }),
    }
}

fn decode_error(error: serde_json::Error) -> Error {
    Error::invalid_data(format!("deserialize history entry: {error}"))
}

enum HistoryEntry {
    Message(Message),
    Compaction(Compaction),
    Request(Request),
}

impl<'de> Deserialize<'de> for HistoryEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HistoryEntryVisitor;

        impl<'de> serde::de::Visitor<'de> for HistoryEntryVisitor {
            type Value = HistoryEntry;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a history record with its discriminator first")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<HistoryEntry, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                if map.next_key::<String>()?.as_deref() != Some("type") {
                    return Err(serde::de::Error::custom(
                        "history discriminator must be the first object property",
                    ));
                }
                let kind = map.next_value::<String>()?;
                // Stream typed fields rather than buffering arbitrary-precision numbers.
                let fields = serde::de::value::MapAccessDeserializer::new(map);
                match kind.as_str() {
                    "message" => Message::deserialize(fields).map(HistoryEntry::Message),
                    "compaction" => Compaction::deserialize(fields).map(HistoryEntry::Compaction),
                    "request" => Request::deserialize(fields).map(HistoryEntry::Request),
                    _ => Err(serde::de::Error::unknown_variant(
                        &kind,
                        &["message", "compaction", "request"],
                    )),
                }
            }
        }

        deserializer.deserialize_map(HistoryEntryVisitor)
    }
}

struct Compaction {
    sequence: i64,
    summary: String,
    watermark: i64,
}

impl<'de> Deserialize<'de> for Compaction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CompactionVisitor;

        impl<'de> serde::de::Visitor<'de> for CompactionVisitor {
            type Value = Compaction;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a compaction history record")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Compaction, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut sequence = 0;
                let mut summary = None;
                let mut watermark = 0;
                while let Some(field) = map.next_key::<String>()? {
                    // Validate every occurrence before replacing a prior valid value.
                    match field.as_str() {
                        "sequence" => sequence = map.next_value::<i64>()?,
                        "summary" => summary = Some(map.next_value::<String>()?),
                        "watermark" => watermark = map.next_value::<i64>()?,
                        _ => {
                            return Err(serde::de::Error::unknown_field(
                                &field,
                                &["sequence", "summary", "watermark"],
                            ));
                        }
                    }
                }
                Ok(Compaction {
                    sequence,
                    summary: summary.ok_or_else(|| serde::de::Error::missing_field("summary"))?,
                    watermark,
                })
            }
        }

        deserializer.deserialize_map(CompactionVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    #[serde(default)]
    sequence: i64,
    #[serde(default)]
    conversation_sequence: i64,
    origin: String,
    role: String,
    parts: Vec<Part>,
    tool_calls: Vec<Call>,
    tool_call_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    kind: String,
    text: String,
    artifact_id: String,
    media_type: String,
    display_name: String,
}

impl Part {
    fn into_part(self) -> Result<ConversationPart> {
        match self.kind.as_str() {
            "text"
                if self.artifact_id.is_empty()
                    && self.media_type.is_empty()
                    && self.display_name.is_empty() =>
            {
                Ok(ConversationPart::Text(self.text))
            }
            "image_artifact" if self.text.is_empty() => Ok(ConversationPart::ImageArtifact {
                artifact_id: self.artifact_id,
                media_type: self.media_type,
                display_name: self.display_name,
            }),
            _ => Err(Error::invalid_data(
                "history part cannot be preserved by the Rust conversation-part model",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    id: String,
    name: String,
    arguments_json: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    sequence: i64,
    #[serde(default)]
    event_sequence: i64,
    request_id: String,
    provider: String,
    model: String,
    effort: Option<String>,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    input_cost: f64,
    #[serde(default)]
    output_cost: f64,
    #[serde(default)]
    tool_calls: i64,
    // Source TotalCost is a getter-only computed property; input is ignored.
    #[serde(default, rename = "total_cost")]
    _total_cost: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_files::json::serialize_entry;

    fn source_observations() -> Vec<Value> {
        let evidence: Value = serde_json::from_str(include_str!(
            "../../tests/history_files/fixtures/format-cases.json"
        ))
        .unwrap();
        let payload = evidence;
        payload["observations"].as_array().unwrap().clone()
    }

    #[test]
    fn decoder_runs_the_three_preserved_source_deserialization_observations() {
        let observations = source_observations();
        for id in [
            "history.deserialize.valid",
            "history.deserialize.unknown-member",
            "history.deserialize.unknown-discriminator",
        ] {
            let observation = observations.iter().find(|row| row["id"] == id).unwrap();
            let input = observation["input"].as_str().unwrap();
            let result = deserialize_entry(input.as_bytes());
            assert_eq!(result.is_ok(), observation["error"] == "none", "{id}");
            if id == "history.deserialize.valid" {
                assert_eq!(
                    result.unwrap(),
                    AgentHistoryEntry::Compaction {
                        sequence: 7,
                        summary: "summary\n雪".into(),
                        watermark: 4,
                    }
                );
            }
        }
    }

    #[test]
    fn decoder_round_trips_all_three_preserved_source_variants() {
        let observations = source_observations();
        let source = observations
            .iter()
            .find(|row| row["id"] == "history.all-discriminators")
            .unwrap()["text"]
            .as_str()
            .unwrap();
        for line in source.lines() {
            let decoded = deserialize_entry(line.as_bytes()).unwrap();
            assert_eq!(serialize_entry(&decoded).unwrap(), line.as_bytes());
        }
    }

    #[test]
    fn request_round_trip_preserves_null_and_high_effort_and_ignores_computed_cost_input() {
        for effort in [None, Some("high".to_owned())] {
            let history = AgentHistoryEntry::Request {
                sequence: 3,
                event_sequence: 15,
                request_id: "grandchild-request".into(),
                provider: "provider".into(),
                model: "model".into(),
                effort,
                input_tokens: 100,
                cached_input_tokens: 25,
                output_tokens: 50,
                input_cost: 0.5,
                output_cost: 0.25,
                tool_calls: 1,
            };
            let bytes = serialize_entry(&history).unwrap();
            let restored = deserialize_entry(&bytes).unwrap();
            assert_eq!(restored, history);
            let mut changed: Value = serde_json::from_slice(&bytes).unwrap();
            changed["total_cost"] = serde_json::json!({"ignored": true});
            let restored = deserialize_entry(&serde_json::to_vec(&changed).unwrap()).unwrap();
            assert_eq!(restored, history);
        }
    }

    #[test]
    fn request_costs_decode_directly_as_numbers_and_reject_number_shaped_objects() {
        for (input, expected) in [
            ("0.125", 0.125_f64),
            ("1e-7", 1e-7),
            ("-0.0", -0.0),
            ("1e17", 1e17),
        ] {
            let json = format!(
                r#"{{"type":"request","request_id":"request","provider":"provider","model":"model","input_cost":{input},"output_cost":{input}}}"#
            );
            let AgentHistoryEntry::Request {
                input_cost,
                output_cost,
                ..
            } = deserialize_entry(json.as_bytes()).unwrap()
            else {
                panic!("expected a request history entry");
            };
            assert_eq!(input_cost.to_bits(), expected.to_bits(), "{input}");
            assert_eq!(output_cost.to_bits(), expected.to_bits(), "{input}");
        }
        for field in ["input_cost", "output_cost"] {
            for input in [
                "null",
                "true",
                "[]",
                "{}",
                r#""0.125""#,
                r#"{"$serde_json::private::Number":"0.125"}"#,
                "1e400",
            ] {
                let json = format!(
                    r#"{{"type":"request","request_id":"request","provider":"provider","model":"model","{field}":{input}}}"#
                );
                assert!(deserialize_entry(json.as_bytes()).is_err(), "{json}");
            }
        }
    }

    #[test]
    fn message_round_trip_preserves_images_text_and_tool_calls() {
        let history = AgentHistoryEntry::Message {
            sequence: 2,
            conversation_sequence: 9,
            origin: "model".into(),
            role: "assistant".into(),
            parts: vec![
                ConversationPart::Text("hello 雪".into()),
                ConversationPart::ImageArtifact {
                    artifact_id: "image-one".into(),
                    media_type: "image/png".into(),
                    display_name: "saved image".into(),
                },
            ],
            tool_calls: vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            tool_call_id: String::new(),
        };
        let bytes = serialize_entry(&history).unwrap();
        assert_eq!(deserialize_entry(&bytes).unwrap(), history);
        for field in ["parts", "tool_calls"] {
            let mut changed: Value = serde_json::from_slice(&bytes).unwrap();
            changed[field][0]["surprise"] = Value::Bool(true);
            assert!(deserialize_entry(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
    }

    #[test]
    fn missing_numeric_constructor_arguments_default_to_zero_but_nulls_do_not() {
        assert_eq!(
            deserialize_entry(br#"{"type":"compaction","summary":"summary"}"#).unwrap(),
            AgentHistoryEntry::Compaction {
                sequence: 0,
                summary: "summary".into(),
                watermark: 0,
            }
        );
        assert!(
            deserialize_entry(br#"{"type":"compaction","summary":"summary","sequence":null}"#)
                .is_err()
        );
    }

    #[test]
    fn decoder_rejects_unrepresentable_null_carriers_and_part_shapes() {
        for input in [
            r#"null"#,
            r#"{"type":"compaction","summary":null}"#,
            r#"{"type":"message","origin":"model","role":"assistant","parts":null,"tool_calls":[],"tool_call_id":""}"#,
            r#"{"type":"message","origin":"model","role":"assistant","parts":[],"tool_calls":[null],"tool_call_id":""}"#,
            r#"{"type":"message","origin":"model","role":"assistant","parts":[{"kind":"other","text":"","artifact_id":"","media_type":"","display_name":""}],"tool_calls":[],"tool_call_id":""}"#,
            r#"{"type":"message","origin":"model","role":"assistant","parts":[{"kind":"text","text":"text","artifact_id":"would-be-lost","media_type":"","display_name":""}],"tool_calls":[],"tool_call_id":""}"#,
            r#"{"summary":"summary","type":"compaction"}"#,
        ] {
            assert!(deserialize_entry(input.as_bytes()).is_err(), "{input}");
        }
    }
}

#[cfg(test)]
mod duplicate_field_regression_tests {
    use super::{AgentHistoryEntry, Deserialize, deserialize_entry};

    #[derive(Deserialize)]
    struct SourceRow {
        id: String,
        input: String,
        outcome: String,
        sequence: Option<i64>,
        summary: Option<String>,
        watermark: Option<i64>,
    }

    #[derive(Deserialize)]
    struct Cases {
        rows: Vec<SourceRow>,
    }

    #[test]
    fn compaction_occurrences_validate_each_duplicate_field() {
        let fixture: Cases = serde_json::from_str(include_str!(
            "../../tests/history_files/fixtures/duplicate-fields.json"
        ))
        .unwrap();
        let mut mismatches = Vec::new();
        for row in &fixture.rows {
            // Pass the original raw input through the actual public decoder unchanged.
            let matched = match (
                row.outcome.as_str(),
                deserialize_entry(row.input.as_bytes()),
            ) {
                (
                    "decoded",
                    Ok(AgentHistoryEntry::Compaction {
                        sequence,
                        summary,
                        watermark,
                    }),
                ) => {
                    Some(sequence) == row.sequence
                        && Some(summary.as_str()) == row.summary.as_deref()
                        && Some(watermark) == row.watermark
                }
                ("exception", Err(_)) => true,
                _ => false,
            };
            if !matched {
                mismatches.push(row.id.as_str());
            }
        }
        assert!(
            mismatches.is_empty(),
            "source-policy mismatch IDs: {mismatches:?}"
        );
    }

    #[test]
    fn compaction_required_default_and_representability_controls_remain_unchanged() {
        assert_eq!(
            deserialize_entry(br#"{"type":"compaction","summary":"kept"}"#).unwrap(),
            AgentHistoryEntry::Compaction {
                sequence: 0,
                summary: "kept".into(),
                watermark: 0,
            }
        );
        // Baseline Rust representability/metadata controls, not additional .NET rows.
        for input in [
            r#"null"#,
            r#"{"type":"compaction"}"#,
            r#"{"type":"compaction","summary":null}"#,
            r#"{"type":"compaction","summary":"kept","sequence":null}"#,
            r#"{"type":"compaction","summary":"kept","watermark":null}"#,
            r#"{"type":"compaction","summary":null,"summary":"later"}"#,
            r#"{"type":"compaction","summary":"kept","sequence":null,"sequence":8}"#,
            r#"{"type":"compaction","summary":"kept","watermark":null,"watermark":4}"#,
            r#"{"summary":"kept","type":"compaction"}"#,
            r#"{"type":"unknown","summary":"kept"}"#,
        ] {
            assert!(deserialize_entry(input.as_bytes()).is_err(), "{input}");
        }
    }
}
