use parrot_domain::protocol::messages::parrot::SetGoalRequest;
use parrot_foundation::config::{ConfigErrorKind, yaml, yaml_json};
use parrot_storage::conversation::AgentHistoryEntry;
use parrot_storage::history_files::json::{deserialize_entry, serialize_entry};
use parrot_tools::task_artifacts::{
    AcceptanceVerdict, TaskPayload, parse_artifact, parse_leaf_response, parse_task_set,
    validate_merged_graph,
};
use protobuf::Message;
use serde_json::{Value, json};

use crate::support::{for_cases, hex};

#[test]
fn seeded_yaml_semantics_malformed_aliases_duplicates_and_tag_boundaries() {
    for invalid in [
        "a: [unterminated",
        "a: *missing\n",
        "a: first\na: second\n",
        "---\na: first\n---\nb: [unterminated",
    ] {
        let error = yaml::parse_root(invalid).unwrap_err();
        assert_eq!(
            error.kind(),
            ConfigErrorKind::InvalidData,
            "input={invalid:?} error={error}"
        );
    }
    assert_eq!(
        yaml_json(&yaml::parse_root("---\n- not-a-map\n---\nsecond: ignored\n").unwrap()).unwrap(),
        "{}"
    );
    assert_eq!(
        yaml_json(&yaml::parse_root("first: retained\n---\nsecond: ignored\n").unwrap()).unwrap(),
        r#"{"first":"retained"}"#
    );
    let invalid_tag = yaml::parse_root("ordinary: !replace [one]\n").unwrap();
    assert!(yaml::validate_replace_tags(&invalid_tag).is_err());
    let large = format!(
        "value: {}\n",
        serde_json::to_string(&format!("text:{}", "x".repeat(65537))).unwrap()
    );
    assert!(
        yaml::parse_root(&large).is_ok(),
        "this syntactic API declares no blanket 64KiB document limit"
    );
    for_cases("yaml", |case, generator| {
        let count = 1 + generator.pick(8);
        let mut input = String::new();
        let mut expected = serde_json::Map::new();
        for index in 0..count {
            let length = generator.pick(48);
            let value = format!("text:{}", generator.text(length));
            input.push_str(&format!(
                "key{index}: {}\n",
                serde_json::to_string(&value).unwrap()
            ));
            expected.insert(format!("key{index}"), Value::String(value));
        }
        let parsed = yaml::parse_root(&input)
            .unwrap_or_else(|error| panic!("{case} input={input:?}: {error}"));
        let before: Value = serde_json::from_str(&yaml_json(&parsed).unwrap()).unwrap();
        let rewritten = yaml::serialize(&parsed);
        let reparsed = yaml::parse_root(&rewritten)
            .unwrap_or_else(|error| panic!("{case} serialized={rewritten:?}: {error}"));
        let after: Value = serde_json::from_str(&yaml_json(&reparsed).unwrap()).unwrap();
        assert_eq!(
            before,
            Value::Object(expected.clone()),
            "{case} input={input:?}"
        );
        assert_eq!(
            after,
            Value::Object(expected),
            "{case} serialized={rewritten:?}"
        );
    });
}

fn task(name: &str, description: &str, payload: &str, dependencies: Vec<String>) -> Value {
    json!({"name":name,"description":description,"payload":payload,"acceptance_criteria":"retain exact result","dependencies":dependencies})
}

#[test]
fn seeded_task_json_envelopes_graphs_duplicates_and_utf16_depth_boundaries() {
    for invalid in [
        r#"[]"#,
        r#"[{"name":"x","name":"x","description":"d","payload":"p","acceptance_criteria":"a"}]"#,
        r#"[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","surprise":1}]"#,
        r#"[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","dependencies":["x"]}]"#,
        r#"[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","dependencies":["a","a"]}]"#,
        r#"[{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","state":"bogus"}]"#,
    ] {
        assert!(
            parse_task_set(invalid).is_err(),
            "malformed task input={invalid}"
        );
    }
    for description in ["a".repeat(300), "🦜".repeat(150)] {
        assert!(
            parse_task_set(&json!([task("x", &description, "p", Vec::new())]).to_string()).is_ok()
        );
    }
    for description in ["a".repeat(301), "🦜".repeat(151)] {
        assert!(
            parse_task_set(&json!([task("x", &description, "p", Vec::new())]).to_string()).is_err()
        );
    }
    let local =
        parse_task_set(&json!([task("x", "d", "p", vec!["later".into()])]).to_string()).unwrap();
    assert!(validate_merged_graph(&local, "tasks").is_err());
    let mut complete = local;
    complete
        .extend(parse_task_set(&json!([task("later", "d", "p", Vec::new())]).to_string()).unwrap());
    validate_merged_graph(&complete, "tasks").unwrap();
    let cyclic = json!({"schema_version":1,"tasks":[task("x","d","p",vec!["y".into()]),task("y","d","p",vec!["x".into()])]});
    assert!(parse_artifact(&cyclic.to_string()).is_err());
    let nested = |depth: usize| {
        format!(
            r#"[{{"name":"x","description":"d","payload":"p","acceptance_criteria":"a","unknown":{}0{}}}]"#,
            "[".repeat(depth),
            "]".repeat(depth)
        )
    };
    assert!(
        parse_task_set(&nested(62))
            .unwrap_err()
            .to_string()
            .contains("unknown field 'unknown'")
    );
    assert_eq!(
        parse_task_set(&nested(63)).unwrap_err().to_string(),
        "Invalid JSON: maximum depth exceeded."
    );
    let precedence = "```json\n{\"result\":\"first\",\"verdict\":\"accept\"}\n```\n```json\n{\"result\":\"second\",\"verdict\":\"reject_and_halt\",\"feedback\":1e400}\n```";
    assert!(
        parse_leaf_response(precedence)
            .unwrap_err()
            .to_string()
            .contains("feedback")
    );
    assert!(
        parse_task_set(&json!([task("x", "d", &"x".repeat(65537), Vec::new())]).to_string())
            .is_ok(),
        "parser has no invented total byte cap"
    );
    for_cases("task_json", |case, generator| {
        let count = 1 + generator.pick(8);
        let mut inputs = Vec::new();
        let mut payloads = Vec::new();
        for index in 0..count {
            let length = generator.pick(48);
            let payload = format!("instruction:{}", generator.text(length));
            let dependencies = if index == 0 {
                Vec::new()
            } else {
                vec![format!("task-{}", index - 1)]
            };
            inputs.push(task(
                &format!("task-{index}"),
                "bounded generated task",
                &payload,
                dependencies,
            ));
            payloads.push(payload);
        }
        let encoded = Value::Array(inputs.clone()).to_string();
        let parsed = parse_task_set(&encoded)
            .unwrap_or_else(|error| panic!("{case} input={encoded}: {error}"));
        validate_merged_graph(&parsed, "tasks").unwrap();
        let artifact = json!({"schema_version":1,"tasks":inputs});
        assert_eq!(
            parse_artifact(&format!("preface\n```json\n{artifact}\n```\n"))
                .unwrap()
                .tasks,
            parsed,
            "{case}"
        );
        for (index, task) in parsed.iter().enumerate() {
            assert_eq!(task.name, format!("task-{index}"), "{case}");
            assert_eq!(
                task.payload,
                TaskPayload::Instruction(payloads[index].clone()),
                "{case}"
            );
        }
        let response = json!({"result":payloads[0],"verdict":"accept"}).to_string();
        let parsed = parse_leaf_response(&response).unwrap();
        assert_eq!(parsed.result, payloads[0], "{case}");
        assert_eq!(parsed.verdict, AcceptanceVerdict::Accept, "{case}");
    });
}

#[test]
fn seeded_history_source_records_unknown_fields_and_finite_roundtrip() {
    let valid = br#"{"type":"compaction","summary":"summary\n\u96ea","watermark":4,"sequence":7}"#;
    assert_eq!(
        deserialize_entry(valid).unwrap(),
        AgentHistoryEntry::Compaction {
            sequence: 7,
            summary: "summary\n雪".into(),
            watermark: 4
        }
    );
    for invalid in [
        br#"{"type":"other"}"#.as_slice(),
        br#"{"type":"compaction","summary":"s","surprise":0}"#.as_slice(),
        b"{\"type\":\"compaction\"".as_slice(),
    ] {
        assert!(
            deserialize_entry(invalid).is_err(),
            "history bytes={invalid:02x?}"
        );
    }
    for_cases("history", |case, generator| {
        let length = generator.pick(48);
        let entry = AgentHistoryEntry::Compaction {
            sequence: generator.next() as i64,
            summary: generator.text(length),
            watermark: generator.next() as i64,
        };
        let encoded = serialize_entry(&entry).unwrap();
        assert_eq!(
            deserialize_entry(&encoded).unwrap_or_else(|error| panic!(
                "{case} input={}: {error}",
                String::from_utf8_lossy(&encoded)
            )),
            entry,
            "{case}"
        );
    });
}

#[test]
fn independent_source_history_duplicate_field_counterexamples_are_regressions() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../crates/parrot-storage/tests/history_files/fixtures/duplicate-fields.json"
    ))
    .unwrap();
    let mut mismatches = Vec::new();
    let rows = corpus["rows"].as_array().unwrap();
    for row in rows {
        let input = row["input"].as_str().unwrap();
        let observed = deserialize_entry(input.as_bytes());
        let matched = if row["outcome"] == "decoded" {
            observed.as_ref().is_ok_and(|entry| {
                *entry
                    == AgentHistoryEntry::Compaction {
                        sequence: row["sequence"].as_i64().unwrap(),
                        summary: row["summary"].as_str().unwrap().into(),
                        watermark: row["watermark"].as_i64().unwrap(),
                    }
            })
        } else {
            assert_eq!(
                row["outcome"], "exception",
                "unexpected independent oracle outcome: {row}"
            );
            observed.is_err()
        };
        if !matched {
            mismatches.push(format!(
                "row={} input={input} source={row} native={observed:?}",
                row["id"]
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "independently observed source policy counterexamples must not be suppressed or retried into success:\n{}",
        mismatches.join("\n")
    );
}

fn varint(mut value: u64, bytes: &mut Vec<u8>) {
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn tag(number: u64, wire: u64, bytes: &mut Vec<u8>) {
    varint((number << 3) | wire, bytes);
}

#[test]
fn seeded_protobuf_typed_mutation_keeps_unknown_occurrences_groups_and_known_state() {
    for invalid in ["a00680", "f3060807", "f3060807fc06", "f406"] {
        assert!(
            SetGoalRequest::parse_from_bytes(&hex(invalid)).is_err(),
            "protobuf malformed regression={invalid}"
        );
    }
    let merged = SetGoalRequest::parse_from_bytes(&hex("1a03a006071a03a00608")).unwrap();
    assert_eq!(merged.write_to_bytes().unwrap(), hex("1a06a00607a00608"));
    let canonical = SetGoalRequest::parse_from_bytes(&hex("a0068100")).unwrap();
    assert_eq!(canonical.write_to_bytes().unwrap(), hex("a00601"));
    let grouped = hex("0a07666978747572651a10f3060807131a06696e7369646514f406");
    let mut grouped_message = SetGoalRequest::parse_from_bytes(&grouped).unwrap();
    grouped_message.user_session_id = "edited".into();
    assert_eq!(
        SetGoalRequest::parse_from_bytes(&grouped_message.write_to_bytes().unwrap()).unwrap(),
        grouped_message
    );
    for_cases("protobuf", |case, generator| {
        let mut bytes = Vec::new();
        let id = format!("session-{}", generator.next());
        tag(1, 2, &mut bytes);
        varint(id.len() as u64, &mut bytes);
        bytes.extend_from_slice(id.as_bytes());
        for _ in 0..2 {
            tag(100, 0, &mut bytes);
            varint(generator.next(), &mut bytes);
        }
        tag(101, 5, &mut bytes);
        bytes.extend_from_slice(&(generator.next() as u32).to_le_bytes());
        tag(102, 1, &mut bytes);
        bytes.extend_from_slice(&generator.next().to_le_bytes());
        let length = generator.pick(17);
        let opaque = generator.bytes(length);
        tag(103, 2, &mut bytes);
        varint(opaque.len() as u64, &mut bytes);
        bytes.extend_from_slice(&opaque);
        tag(104, 3, &mut bytes);
        tag(1, 0, &mut bytes);
        varint(generator.next(), &mut bytes);
        tag(104, 4, &mut bytes);
        let mut message = SetGoalRequest::parse_from_bytes(&bytes)
            .unwrap_or_else(|error| panic!("{case} hex={bytes:02x?}: {error}"));
        assert_eq!(message.user_session_id, id, "{case} hex={bytes:02x?}");
        assert_eq!(
            message.special_fields.unknown_fields().iter().count(),
            6,
            "{case} hex={bytes:02x?}"
        );
        message.user_session_id.push_str("-edited");
        let rewritten = message.write_to_bytes().unwrap();
        assert_eq!(
            SetGoalRequest::parse_from_bytes(&rewritten).unwrap(),
            message,
            "{case} original={bytes:02x?} rewritten={rewritten:02x?}"
        );
        let length = generator.pick(513);
        let arbitrary = generator.bytes(length);
        if let Ok(mut accepted) = SetGoalRequest::parse_from_bytes(&arbitrary) {
            accepted.user_session_id = id;
            let encoded = accepted.write_to_bytes().unwrap();
            assert_eq!(
                SetGoalRequest::parse_from_bytes(&encoded).unwrap(),
                accepted,
                "{case} arbitrary={arbitrary:02x?}"
            );
        }
    });
}
