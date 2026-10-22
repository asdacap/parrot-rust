use parrot_tools::agents::input::*;

#[test]
fn spawn_preserves_empty_values_and_named_forks() {
    let input =
        SpawnInput::parse(r#"{"prompt":"","agent":"","name":"  Child Helper!  "}"#).unwrap();
    assert_eq!(input.prompt, "");
    assert_eq!(input.agent, "");
    assert_eq!(input.name, "  Child Helper!  ");
    assert_eq!(input.model, "");
    assert_eq!(input.scope, "");
    assert_eq!(input.fork, ForkSelection::Empty);
    for (wire, expected) in [
        ("empty", ForkSelection::Empty),
        ("full", ForkSelection::Full),
        ("checkpoint", ForkSelection::Named("checkpoint".into())),
        ("Full", ForkSelection::Named("Full".into())),
    ] {
        let value = serde_json::json!({"prompt":"p","agent":"worker","name":"n","fork":wire});
        assert_eq!(
            SpawnInput::parse(&value.to_string()).unwrap().fork,
            expected
        );
    }
}

#[test]
fn spawn_requires_name_and_rejects_unknown_or_nonstring_values() {
    for arguments in [
        r#"{"prompt":"p","agent":"worker"}"#,
        r#"{"prompt":"p","agent":"worker","name":""}"#,
        r#"{"prompt":"p","agent":"worker","name":null}"#,
    ] {
        assert_eq!(
            SpawnInput::parse(arguments).unwrap_err().0,
            "Tool arguments require a string 'name'."
        );
    }
    for arguments in [
        "[]",
        "null",
        "not-json",
        r#"{"prompt":7,"agent":"worker","name":"n"}"#,
        r#"{"prompt":"p","agent":"worker","name":"n","extra":true}"#,
    ] {
        assert!(SpawnInput::parse(arguments).is_err());
    }
}

#[test]
fn send_enforces_utf8_byte_boundary_before_any_service() {
    for message in ["x".repeat(32768), format!("{}x", "界".repeat(10922))] {
        let args = serde_json::json!({"name":"worker", "message":message});
        assert_eq!(
            SendInput::parse(&args.to_string()).unwrap().message,
            message
        );
    }
    for message in ["x".repeat(32769), "界".repeat(10923)] {
        let args = serde_json::json!({"name":"worker", "message":message});
        assert_eq!(
            SendInput::parse(&args.to_string()).unwrap_err().0,
            "agent message exceeds 32768 UTF-8 bytes; split it into smaller messages"
        );
    }
    assert_eq!(
        SendInput::parse(r#"{"name":"parent/parent","message":"  "}"#)
            .unwrap()
            .message,
        "  "
    );
}

#[test]
fn interrupt_trims_but_status_retains_address_spelling() {
    assert_eq!(
        parse_interrupt_name(r#"{"name":"  worker  "}"#).unwrap(),
        "worker"
    );
    assert_eq!(
        parse_status_name(r#"{"name":"  worker  "}"#).unwrap(),
        "  worker  "
    );
    for args in [r#"{"name":""}"#, r#"{"name":"  "}"#] {
        assert_eq!(
            parse_interrupt_name(args).unwrap_err().0,
            "no child agent given"
        );
        assert_eq!(
            parse_status_name(args).unwrap_err().0,
            "no child agent given"
        );
    }
    assert!(parse_interrupt_name(r#"{"name":"worker","extra":1}"#).is_err());
    assert!(parse_status_name(r#"{"name":"worker","extra":1}"#).is_err());
}

#[test]
fn wait_default_limits_and_strict_integer_shapes() {
    assert_eq!(WaitInput::parse("{}").unwrap().duration_ms, 300000);
    for duration in [10000_u64, 4294967294] {
        assert_eq!(
            WaitInput::parse(&format!("{{\"duration_ms\":{duration}}}"))
                .unwrap()
                .duration_ms,
            duration
        );
    }
    for args in [
        "[]",
        "null",
        r#"{"unexpected":true}"#,
        r#"{"duration_ms":9999}"#,
        r#"{"duration_ms":-1}"#,
        r#"{"duration_ms":10000.5}"#,
        r#"{"duration_ms":10000.0}"#,
        r#"{"duration_ms":1e4}"#,
        r#"{"duration_ms":"10000"}"#,
        r#"{"duration_ms":4294967295}"#,
        r#"{"duration_ms":null}"#,
    ] {
        assert!(WaitInput::parse(args).is_err(), "{args}");
    }
}

#[test]
fn question_options_preserve_labels_and_descriptions() {
    let questions = parse_questions(r#"{"questions":[{"header":"Palette","prompt":"Pick","options":["Blue",{"label":"Red","description":"warm"}],"multiple":true,"custom":true}]}"#).unwrap();
    let question = &questions[0];
    assert_eq!(question.header.as_deref(), Some("Palette"));
    assert_eq!(
        question.options.as_ref().unwrap(),
        &[
            WireQuestionOption {
                label: "Blue".into(),
                description: String::new()
            },
            WireQuestionOption {
                label: "Red".into(),
                description: "warm".into()
            },
        ]
    );
    assert!(question.multiple);
    assert_eq!(question.custom, Some(true));
}

#[test]
fn questions_and_options_reject_all_unknown_fields() {
    for args in [
        r#"{"questions":[],"unexpected":true}"#,
        r#"{"questions":[{"id":"colour","prompt":"Pick","options":[{"id":"blue","label":"Blue"}],"unexpected":true}]}"#,
        r#"{"questions":[{"id":"colour","prompt":"Pick","options":[{"id":"blue","label":"Blue","unexpected":true}]}]}"#,
        r#"{"questions":[{"prompt":"p","options":[{"label":"x","extra":1}]}]}"#,
    ] {
        assert!(parse_questions(args).is_err(), "{args}");
    }
}

#[test]
fn options_reject_blank_wrong_typed_or_missing_labels() {
    for option in [
        r#"""#,
        r#""  ""#,
        "42",
        "true",
        "[]",
        "{}",
        r#"{"label":" "}"#,
        r#"{"label":null}"#,
        r#"{"label":1}"#,
        r#"{"label":"x","description":null}"#,
    ] {
        let args = format!("{{\"questions\":[{{\"prompt\":\"Pick\",\"options\":[{option}]}}]}}");
        assert!(parse_questions(&args).is_err(), "{args}");
    }
}

#[test]
fn repeated_wire_fields_keep_the_last_source_serializer_value() {
    let questions = parse_questions(r#"{"questions":[],"questions":[{"prompt":"first","prompt":"last","options":[{"label":"old","label":"new"}]}]}"#).unwrap();
    assert_eq!(questions[0].prompt.as_deref(), Some("last"));
    assert_eq!(questions[0].options.as_ref().unwrap()[0].label, "new");
}

#[test]
fn invalid_earlier_deserialized_fields_cannot_be_repaired_by_duplicate_later_values() {
    assert!(SpawnInput::parse(r#"{"prompt":7,"prompt":"p","agent":"worker","name":"n"}"#).is_err());
    assert_eq!(
        SpawnInput::parse(r#"{"scope":7}"#).unwrap_err().0,
        "Tool argument 'scope' must be a string."
    );
    assert!(SendInput::parse(r#"{"name":"worker","message":7,"message":"valid"}"#).is_err());
    assert!(parse_status_name(r#"{"name":7,"name":"worker"}"#).is_err());
    assert!(parse_interrupt_name(r#"{"name":7,"name":"worker"}"#).is_err());
    for arguments in [
        r#"{"questions":7,"questions":[{"prompt":"p"}]}"#,
        r#"{"questions":[{"prompt":7,"prompt":"p"}]}"#,
        r#"{"questions":[{"prompt":"p","multiple":null,"multiple":true}]}"#,
        r#"{"questions":[{"prompt":"p","options":[{"label":7,"label":"ok"}]}]}"#,
    ] {
        assert!(parse_questions(arguments).is_err(), "{arguments}");
    }
    // Wait uses JsonDocument/TryGetProperty, unlike the deserialized tool inputs.
    assert_eq!(
        WaitInput::parse(r#"{"duration_ms":"bad","duration_ms":10000}"#)
            .unwrap()
            .duration_ms,
        10000
    );
}

#[test]
fn ask_and_reply_require_nonblank_plain_text_and_preserve_resolver_paths() {
    for name in [
        "parent",
        "parent/sibling",
        "/root-name/child",
        "child/grandchild",
    ] {
        let input = AskInput::parse(
            &serde_json::json!({"name":name,"message":"  plain\ntext  "}).to_string(),
        )
        .unwrap();
        assert_eq!(input.name, name);
        assert_eq!(input.message, "  plain\ntext  ");
    }
    assert_eq!(
        ReplyInput::parse(r#"{"request_id":"request","message":"  yes  "}"#)
            .unwrap()
            .message,
        "  yes  "
    );
    for value in [
        serde_json::json!(null),
        serde_json::json!(""),
        serde_json::json!(" \n "),
        serde_json::json!(42),
        serde_json::json!(["yes"]),
    ] {
        for field in ["name", "message"] {
            let mut args = serde_json::json!({"name":"parent","message":"question"});
            args[field] = value.clone();
            assert!(AskInput::parse(&args.to_string()).is_err());
        }
        for field in ["request_id", "message"] {
            let mut args = serde_json::json!({"request_id":"request","message":"yes"});
            args[field] = value.clone();
            assert!(ReplyInput::parse(&args.to_string()).is_err());
        }
    }
    assert!(AskInput::parse(r#"{"name":"parent","message":"x","extra":true}"#).is_err());
    assert!(ReplyInput::parse(r#"{"request_id":"request","message":"x","extra":true}"#).is_err());
    assert!(AskInput::parse(r#"{"name":"parent","message":7,"message":"valid"}"#).is_err());
    assert!(
        ReplyInput::parse(r#"{"request_id":7,"request_id":"request","message":"valid"}"#).is_err()
    );
}
