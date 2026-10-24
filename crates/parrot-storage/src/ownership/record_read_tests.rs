use super::*;
use std::{fs, path::PathBuf, sync::Arc};

const WORKSPACE: &str = "/tmp/c/workspace/physical";
const RECORD: &str = r#"{"SchemaVersion":2,"Version":1,"Generation":1,"State":"Active","SessionId":"user-session-boundary","WorkingDirectory":"/tmp/c/workspace/physical","HostKey":"host","ProcessId":4312,"ProcessStartToken":"start"}"#;

fn expected_record() -> OwnerRecord {
    OwnerRecord {
        schema_version: Some(2),
        version: Some(1),
        generation: Some(1),
        state: Some("Active".into()),
        session_id: Some("user-session-boundary".into()),
        working_directory: Some(WORKSPACE.into()),
        host_key: Some("host".into()),
        process_id: Some(4312),
        process_start_token: Some("start".into()),
        ..OwnerRecord::default()
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "t21-record-read-{}",
            super::runtime::random_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_current(bytes: &[u8], expected: Option<OwnerRecord>) {
    let fixture = Fixture::new();
    let path = fixture.0.join("v1.json");
    fs::write(&path, bytes).unwrap();
    assert_eq!(
        WorkingDirectoryClaim::current(&fixture.0).unwrap(),
        expected
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

fn assert_record(text: &str, expected: Option<OwnerRecord>) {
    assert_eq!(
        serde_json::from_str::<OwnerRecord>(text).ok(),
        expected,
        "{text}"
    );
    // Deserializable records with absent or nonpositive versions are not heads.
    assert_current(
        text.as_bytes(),
        expected.filter(|record| record.version.is_some_and(|version| version > 0)),
    );
}

const ENCODINGS: [(u32, &[u8]); 5] = [
    (65001, b"\xef\xbb\xbf"),
    (1200, b"\xff\xfe"),
    (1201, b"\xfe\xff"),
    (12000, b"\xff\xfe\0\0"),
    (12001, b"\0\0\xfe\xff"),
];

fn encoded_record(code_page: u32, preamble: &[u8]) -> Vec<u8> {
    let text = format!(
        r#"{},"UnicodeMarker":"é雪𝄞"}}"#,
        &RECORD[..RECORD.len() - 1]
    );
    let mut bytes = preamble.to_vec();
    match code_page {
        65001 => bytes.extend(text.as_bytes()),
        1200 => bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes)),
        1201 => bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes)),
        12000 => bytes.extend(text.chars().flat_map(|c| u32::from(c).to_le_bytes())),
        12001 => bytes.extend(text.chars().flat_map(|c| u32::from(c).to_be_bytes())),
        other => panic!("unexpected encoding {other}"),
    }
    bytes
}

#[test]
fn owner_reader_detects_all_boms_but_only_utf8_without_a_bom() {
    for (code_page, preamble) in ENCODINGS {
        assert_current(
            &encoded_record(code_page, preamble),
            Some(expected_record()),
        );
        let expected = if code_page == 65001 {
            Some(expected_record())
        } else {
            None
        };
        assert_current(&encoded_record(code_page, &[]), expected);
    }
}

#[test]
fn nullable_integer_boundaries_preserve_fields_and_positive_version_selection() {
    // i32 fields reject the four middle values that remain valid for Generation.
    let integers = [
        ("null", None),
        ("-0", Some(0)),
        ("0", Some(0)),
        ("-1", Some(-1)),
        ("1", Some(1)),
        ("2147483647", Some(2_147_483_647)),
        ("-2147483648", Some(-2_147_483_648)),
        ("2147483648", Some(2_147_483_648)),
        ("-2147483649", Some(-2_147_483_649)),
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
    ];
    for (key, original) in [
        ("SchemaVersion", "2"),
        ("Version", "1"),
        ("Generation", "1"),
        ("ProcessId", "4312"),
    ] {
        for (literal, value) in integers {
            let text = RECORD.replace(
                &format!(r#""{key}":{original}"#),
                &format!(r#""{key}":{literal}"#),
            );
            let mut expected = expected_record();
            let narrow = value.map(i32::try_from).transpose();
            if key != "Generation" && narrow.is_err() {
                assert_record(&text, None);
                continue;
            }
            match key {
                "SchemaVersion" => expected.schema_version = narrow.unwrap(),
                "Version" => expected.version = narrow.unwrap(),
                "Generation" => expected.generation = value,
                "ProcessId" => expected.process_id = narrow.unwrap(),
                _ => unreachable!(),
            }
            assert_record(&text, Some(expected));
        }
        for literal in [
            "9223372036854775808",
            "-9223372036854775809",
            "0.0",
            "-0.0",
            "0e0",
            "0E+0",
            "1e0",
            r#""0""#,
            "true",
            "{}",
            "[]",
            "+0",
            "00",
            "NaN",
        ] {
            let text = RECORD.replace(
                &format!(r#""{key}":{original}"#),
                &format!(r#""{key}":{literal}"#),
            );
            assert_record(&text, None);
        }
    }
}

#[test]
fn duplicate_known_properties_validate_each_occurrence_and_keep_the_last() {
    for (property, value) in [
        ("Version", Some(2)),
        ("Version", Some(0)),
        ("Version", None),
        ("SchemaVersion", Some(999)),
        ("Generation", None),
        ("ProcessId", None),
    ] {
        let literal = value.map_or_else(|| "null".to_owned(), |value| value.to_string());
        let text = format!(
            r#"{},"{property}":{literal}}}"#,
            &RECORD[..RECORD.len() - 1]
        );
        let mut expected = expected_record();
        match property {
            "Version" => expected.version = value,
            "SchemaVersion" => expected.schema_version = value,
            "Generation" => expected.generation = value.map(i64::from),
            "ProcessId" => expected.process_id = value,
            _ => unreachable!(),
        }
        assert_record(&text, Some(expected));
    }
    let null_pid = RECORD.replace(r#""ProcessId":4312"#, r#""ProcessId":null"#);
    assert_record(
        &format!(r#"{},"ProcessId":4312}}"#, &null_pid[..null_pid.len() - 1]),
        Some(expected_record()),
    );
    for (first, last) in [("0.0", "2"), ("1", "0e0")] {
        let text = RECORD.replace(r#""Version":1"#, &format!(r#""Version":{first}"#));
        assert_record(
            &format!(r#"{},"Version":{last}}}"#, &text[..text.len() - 1]),
            None,
        );
    }
    let text = RECORD.replace(r#""ProcessId":4312"#, r#""ProcessId":"4312""#);
    assert_record(
        &format!(r#"{},"ProcessId":4312}}"#, &text[..text.len() - 1]),
        None,
    );
    assert_record(
        &format!(r#"{},"version":0.0}}"#, &RECORD[..RECORD.len() - 1]),
        Some(expected_record()),
    );
}

#[test]
fn unknown_containers_obey_the_64_level_record_depth_boundary() {
    for kind in ["object", "array", "mixed"] {
        for depth in [0, 1, 60, 61, 62, 63, 64, 65] {
            let mut value = "null".to_owned();
            for level in 0..depth {
                value = if kind == "object" || (kind == "mixed" && level % 2 == 0) {
                    format!(r#"{{"x":{value}}}"#)
                } else {
                    format!("[{value}]")
                };
            }
            let text = format!(r#"{},"Future":{value}}}"#, &RECORD[..RECORD.len() - 1]);
            let expected = if depth <= 63 {
                Some(expected_record())
            } else {
                None
            };
            assert_record(&text, expected);
        }
    }
    let brackets = "{[".repeat(50);
    assert_record(
        &format!(r#"{},"Future":"{brackets}"}}"#, &RECORD[..RECORD.len() - 1]),
        Some(expected_record()),
    );
}

#[test]
fn bom_live_workspace_owner_in_either_root_blocks_both_takeover_and_fallback_settings() {
    for (code_page, preamble) in ENCODINGS {
        for current_owner in [false, true] {
            for fallback in [false, true] {
                for takeover in [false, true] {
                    let fixture = Fixture::new();
                    let current = fixture.0.join("parrotrust");
                    let legacy = fixture.0.join("parrotdotnet");
                    let owner_root = if current_owner { &current } else { &legacy };
                    let workspace = std::path::Path::new(WORKSPACE);
                    let canonical = WorkingDirectoryClaim::canonicalize(workspace).unwrap();
                    let ledger = owner_root
                        .join("owners")
                        .join(WorkingDirectoryClaim::fingerprint(&canonical));
                    fs::create_dir_all(&ledger).unwrap();
                    let bytes = encoded_record(code_page, preamble);
                    fs::write(ledger.join("v1.json"), &bytes).unwrap();
                    let runtime = RuntimeIdentity::new(
                        fingerprint_host("host"),
                        "synthetic-boot".into(),
                        4312,
                        "different-reader-start".into(),
                        "synthetic-reader".into(),
                        Arc::new(|_| ProcessIdentity {
                            status: ProcessIdentityStatus::Found,
                            process_start_token: Some("different-reader-start".into()),
                        }),
                    );
                    let admissions = SessionAdmissions::new(
                        OwnershipRoots::new(current.clone(), legacy.clone()).unwrap(),
                        runtime,
                        fallback,
                    );
                    let id = parrot_domain::values::identifiers::UserSessionId::parse(
                        "user-session-boundary",
                    )
                    .unwrap();
                    let refused = admissions.resume(workspace, &id, takeover).unwrap();
                    assert_eq!(refused.admission.disposition, ClaimDisposition::Live);
                    assert!(refused.admission.activation_lease.is_none());
                    let fresh = admissions.create_fresh(workspace, &id).unwrap();
                    assert_eq!(fresh.admission.disposition, ClaimDisposition::Live);
                    assert!(fresh.admission.activation_lease.is_none());
                    assert_eq!(fs::read(ledger.join("v1.json")).unwrap(), bytes);
                    for root in [&current, &legacy] {
                        assert!(!root.join("sessions").exists());
                        assert!(!root.join("owners/activations").exists());
                    }
                }
            }
        }
    }
}

#[test]
fn unknown_scalar_numbers_are_skipped_without_floating_point_range_conversion() {
    for value in [
        "1e400",
        "-1e400",
        "9999999999999999999999999999999999999999999",
        "-0",
        "true",
        "false",
        "null",
        r#""quoted { [ brackets ] }""#,
    ] {
        let text = format!(r#"{{"Version":1,"Future":{value}}}"#);
        assert_eq!(
            serde_json::from_str::<OwnerRecord>(&text).unwrap().version,
            Some(1)
        );
        let nested = format!(r#"{{"Version":1,"Future":[{{"nested":{value}}}]}}"#);
        assert_eq!(
            serde_json::from_str::<OwnerRecord>(&nested)
                .unwrap()
                .version,
            Some(1)
        );
    }
}

#[test]
fn nullable_integer_lexemes_validate_every_occurrence_before_last_wins() {
    for key in ["SchemaVersion", "Version", "Generation", "ProcessId"] {
        let zero = format!(r#"{{"{key}":-0}}"#);
        assert_eq!(
            serde_json::to_value(serde_json::from_str::<OwnerRecord>(&zero).unwrap()).unwrap()[key],
            0
        );
        for invalid in [
            "-0.0",
            "-0e0",
            "0.0",
            "0e0",
            "true",
            r#""0""#,
            "[]",
            "{}",
            "9223372036854775808",
        ] {
            for text in [
                format!(r#"{{"{key}":{invalid},"{key}":1}}"#),
                format!(r#"{{"{key}":1,"{key}":{invalid}}}"#),
            ] {
                assert!(
                    serde_json::from_str::<OwnerRecord>(&text).is_err(),
                    "{text}"
                );
            }
        }
        let final_null = format!(r#"{{"{key}":-0,"{key}":null}}"#);
        assert!(
            serde_json::to_value(serde_json::from_str::<OwnerRecord>(&final_null).unwrap())
                .unwrap()[key]
                .is_null()
        );
    }
}
