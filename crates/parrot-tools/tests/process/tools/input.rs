use parrot_tools::process::tools::input::{
    ExecInput, InputError, InterruptInput, MonitorInput, WriteStdinInput,
};

fn error<T: std::fmt::Debug>(result: Result<T, InputError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn exec_defaults_and_nullable_optional_fields() {
    let input = ExecInput::parse(r#"{"command":"true"}"#).unwrap();
    assert_eq!(input.command, "true");
    assert!(input.environment.is_empty());
    assert_eq!(input.name, None);
    assert_eq!(input.description, None);
    assert_eq!(input.yield_after_ms, None);
    assert!(!input.tty);
    let nullable = ExecInput::parse(
        r#"{"command":"true","env":null,"name":null,"description":null,"yield_after_ms":null}"#,
    )
    .unwrap();
    assert_eq!(nullable, input);
}

#[test]
fn exec_alias_falls_back_only_for_missing_or_null_command() {
    for json in [r#"{"cmd":"alias"}"#, r#"{"command":null,"cmd":"alias"}"#] {
        assert_eq!(ExecInput::parse(json).unwrap().command, "alias");
    }
    for json in [
        r#"{"command":"primary","cmd":"alias"}"#,
        r#"{"cmd":"alias","command":"primary"}"#,
    ] {
        assert_eq!(ExecInput::parse(json).unwrap().command, "primary");
    }
    assert_eq!(
        ExecInput::parse(r#"{"command":"","cmd":"alias"}"#)
            .unwrap()
            .command,
        ""
    );
    assert!(ExecInput::parse(r#"{"command":1,"cmd":"alias"}"#).is_err());
    assert!(ExecInput::parse(r#"{"command":"primary","cmd":1}"#).is_err());
}

#[test]
fn missing_required_strings_have_source_messages() {
    for json in [
        "{}",
        r#"{"command":null,"cmd":null}"#,
        "null",
        "[]",
        "42",
        "true",
        r#""text""#,
    ] {
        assert_eq!(
            error(ExecInput::parse(json)),
            "Tool arguments require a string 'command'."
        );
    }
    for json in ["{}", r#"{"command":null}"#, "null", "[]"] {
        assert_eq!(
            error(MonitorInput::parse(json)),
            "Tool arguments require a string 'command'."
        );
    }
    for json in ["{}", r#"{"name":null,"input":"text"}"#, "null", "[]"] {
        assert_eq!(
            error(WriteStdinInput::parse(json)),
            "Tool arguments require a string 'name'."
        );
        assert_eq!(
            error(InterruptInput::parse(json)),
            "Tool arguments require a string 'name'."
        );
    }
    for json in [r#"{"name":"p"}"#, r#"{"name":"p","input":null}"#] {
        assert_eq!(
            error(WriteStdinInput::parse(json)),
            "Tool arguments require a string 'input'."
        );
    }
}

#[test]
fn names_and_empty_strings_remain_for_wrapper_validation() {
    let exec = ExecInput::parse(r#"{"command":"","name":"  p  ","description":""}"#).unwrap();
    assert_eq!(exec.command, "");
    assert_eq!(exec.name.as_deref(), Some("  p  "));
    assert_eq!(exec.description.as_deref(), Some(""));
    let write = WriteStdinInput::parse(r#"{"name":"  ","input":""}"#).unwrap();
    assert_eq!(write.name, "  ");
    assert_eq!(write.input, "");
    assert_eq!(
        InterruptInput::parse(r#"{"name":"  "}"#).unwrap().name,
        "  "
    );
}

#[test]
fn tty_accepts_only_nonnullable_boolean() {
    assert!(
        ExecInput::parse(r#"{"command":"true","tty":true}"#)
            .unwrap()
            .tty
    );
    assert!(
        !ExecInput::parse(r#"{"command":"true","tty":false}"#)
            .unwrap()
            .tty
    );
    for value in ["null", "0", "1", r#""true""#, "[]", "{}"] {
        assert!(ExecInput::parse(&format!(r#"{{"command":"true","tty":{value}}}"#)).is_err());
    }
}

#[test]
fn optional_strings_reject_nonstring_types() {
    for value in ["false", "0", "[]", "{}"] {
        for name in ["name", "description"] {
            assert!(
                ExecInput::parse(&format!(r#"{{"command":"true","{name}":{value}}}"#)).is_err()
            );
            assert!(
                MonitorInput::parse(&format!(r#"{{"command":"true","{name}":{value}}}"#)).is_err()
            );
        }
        assert!(WriteStdinInput::parse(&format!(r#"{{"name":"p","input":{value}}}"#)).is_err());
        assert!(InterruptInput::parse(&format!(r#"{{"name":{value}}}"#)).is_err());
    }
}

#[test]
fn unknown_properties_follow_the_source_generation_context() {
    for property in ["unexpected", "Command", "TTY"] {
        assert!(ExecInput::parse(&format!(r#"{{"command":"true","{property}":true}}"#)).is_err());
        assert!(
            MonitorInput::parse(&format!(r#"{{"command":"true","{property}":true}}"#)).is_err()
        );
    }
    let write = WriteStdinInput::parse(r#"{"name":"p","input":"text","unexpected":true}"#).unwrap();
    assert_eq!(write.input, "text");
    assert_eq!(
        InterruptInput::parse(r#"{"name":"p","Signal":9,"unexpected":true}"#)
            .unwrap()
            .signal,
        2
    );
    assert_eq!(
        error(WriteStdinInput::parse(r#"{"Name":"p","input":"text"}"#)),
        "Tool arguments require a string 'name'."
    );
}

#[test]
fn environment_values_are_literal_strings_and_duplicate_keys_use_last_value() {
    let input = ExecInput::parse(
        r#"{"command":"true","env":{"VALUE":"first","VALUE":"last","EMPTY":"","PATH_VALUE":"${WORKDIR}/literal path","quoted":"$(printf injected); * ' quoted"}}"#,
    )
    .unwrap();
    assert_eq!(input.environment["VALUE"], "last");
    assert_eq!(input.environment["EMPTY"], "");
    assert_eq!(input.environment["PATH_VALUE"], "${WORKDIR}/literal path");
    assert_eq!(
        input.environment["quoted"],
        "$(printf injected); * ' quoted"
    );
}

#[test]
fn environment_rejects_nonobjects_with_exact_message() {
    for value in ["[]", "true", "42", r#""text""#] {
        let expected = "Tool argument 'env' must be an object containing string values.";
        assert_eq!(
            error(ExecInput::parse(&format!(
                r#"{{"command":"true","env":{value}}}"#
            ))),
            expected
        );
        assert_eq!(
            error(MonitorInput::parse(&format!(
                r#"{{"command":"true","env":{value}}}"#
            ))),
            expected
        );
    }
}

#[test]
fn environment_rejects_nonstring_values_with_exact_message() {
    for value in ["null", "1", "true", "[]", "{}"] {
        let expected = "Tool argument 'env' must contain only string values.";
        assert_eq!(
            error(ExecInput::parse(&format!(
                r#"{{"command":"true","env":{{"VALUE":{value}}}}}"#
            ))),
            expected
        );
        assert_eq!(
            error(MonitorInput::parse(&format!(
                r#"{{"command":"true","env":{{"VALUE":{value}}}}}"#
            ))),
            expected
        );
    }
}

#[test]
fn environment_rejects_invalid_names_and_nul_values_with_exact_message() {
    for env in [
        r#"{"":"value"}"#,
        r#"{"INVALID=NAME":"value"}"#,
        r#"{"NUL\u0000NAME":"value"}"#,
        r#"{"VALUE":"has\u0000nul"}"#,
    ] {
        let expected = "Tool argument 'env' contains an invalid environment value.";
        assert_eq!(
            error(ExecInput::parse(&format!(
                r#"{{"command":"true","env":{env}}}"#
            ))),
            expected
        );
        // Monitor constructs ProcessEnvironmentOverrides only after its
        // command/description/name/timeout checks; the wrapper validates these.
        assert!(MonitorInput::parse(&format!(r#"{{"command":"true","env":{env}}}"#)).is_ok());
    }
}

#[test]
fn yield_delay_defaults_and_inclusive_bounds() {
    assert_eq!(
        WriteStdinInput::parse(r#"{"name":"p","input":""}"#)
            .unwrap()
            .yield_after_ms,
        250
    );
    assert_eq!(
        WriteStdinInput::parse(r#"{"name":"p","input":"","yield_after_ms":null}"#)
            .unwrap()
            .yield_after_ms,
        250
    );
    for (lexeme, expected) in [("0", 0), ("-0", 0), ("4294967294", 4_294_967_294)] {
        assert_eq!(
            ExecInput::parse(&format!(
                r#"{{"command":"true","yield_after_ms":{lexeme}}}"#
            ))
            .unwrap()
            .yield_after_ms,
            Some(expected)
        );
        assert_eq!(
            WriteStdinInput::parse(&format!(
                r#"{{"name":"p","input":"","yield_after_ms":{lexeme}}}"#
            ))
            .unwrap()
            .yield_after_ms,
            expected
        );
    }
}

#[test]
fn yield_delay_rejects_out_of_bounds_with_exact_message() {
    for lexeme in [
        "-1",
        "4294967295",
        "9223372036854775807",
        "-9223372036854775808",
    ] {
        let expected = "Tool argument 'yield_after_ms' must be a non-negative integer no greater than 4294967294.";
        assert_eq!(
            error(ExecInput::parse(&format!(
                r#"{{"command":"true","yield_after_ms":{lexeme}}}"#
            ))),
            expected
        );
        assert_eq!(
            error(WriteStdinInput::parse(&format!(
                r#"{{"name":"p","input":"","yield_after_ms":{lexeme}}}"#
            ))),
            expected
        );
    }
}

#[test]
fn integer_properties_do_not_coerce_number_lexemes_or_types() {
    for lexeme in [
        "1.0",
        "1e0",
        "1E+0",
        "1.5",
        "9223372036854775808",
        "-9223372036854775809",
        r#""1""#,
        "true",
        "[]",
        "{}",
    ] {
        assert!(
            ExecInput::parse(&format!(
                r#"{{"command":"true","yield_after_ms":{lexeme}}}"#
            ))
            .is_err()
        );
        assert!(
            WriteStdinInput::parse(&format!(
                r#"{{"name":"p","input":"","yield_after_ms":{lexeme}}}"#
            ))
            .is_err()
        );
        assert!(
            MonitorInput::parse(&format!(r#"{{"command":"true","timeout_ms":{lexeme}}}"#)).is_err()
        );
    }
}

#[test]
fn interrupt_defaults_and_inclusive_signal_range() {
    for json in [r#"{"name":"p"}"#, r#"{"name":"p","signal":null}"#] {
        assert_eq!(InterruptInput::parse(json).unwrap().signal, 2);
    }
    for signal in [1, 64] {
        assert_eq!(
            InterruptInput::parse(&format!(r#"{{"name":"p","signal":{signal}}}"#))
                .unwrap()
                .signal,
            signal
        );
    }
}

#[test]
fn interrupt_signal_lexemes_and_int32_bounds_have_exact_message() {
    for lexeme in [
        "1.0",
        "1e0",
        "1E+0",
        "1.5",
        "2147483648",
        "-2147483649",
        "999999999999999999999",
        r#""SIGINT""#,
        "false",
        "[]",
        "{}",
    ] {
        assert_eq!(
            error(InterruptInput::parse(&format!(
                r#"{{"name":"p","signal":{lexeme}}}"#
            ))),
            "Tool argument 'signal' must be an integer."
        );
    }
    assert_eq!(
        error(InterruptInput::parse(r#"{"signal":1.0}"#)),
        "Tool argument 'signal' must be an integer."
    );
}

#[test]
fn interrupt_out_of_range_integers_have_exact_message() {
    for lexeme in ["0", "-0", "-1", "65", "2147483647", "-2147483648"] {
        assert_eq!(
            error(InterruptInput::parse(&format!(
                r#"{{"name":"p","signal":{lexeme}}}"#
            ))),
            "Tool argument 'signal' must be between 1 and 64."
        );
    }
}

#[test]
fn monitor_defaults_nullable_fields_and_wrapper_validation_values() {
    let input = MonitorInput::parse(r#"{"command":"true"}"#).unwrap();
    assert_eq!(input.description, None);
    assert_eq!(input.name, None);
    assert_eq!(input.timeout_ms, 300_000);
    assert!(input.environment.is_empty());
    assert_eq!(
        MonitorInput::parse(
            r#"{"command":"true","description":null,"name":null,"env":null,"timeout_ms":null}"#
        )
        .unwrap(),
        input
    );
    let empty =
        MonitorInput::parse(r#"{"command":"","description":" ","name":"  p  ","timeout_ms":999}"#)
            .unwrap();
    assert_eq!(empty.command, "");
    assert_eq!(empty.description.as_deref(), Some(" "));
    assert_eq!(empty.name.as_deref(), Some("  p  "));
    assert_eq!(empty.timeout_ms, 999);
    assert_eq!(
        MonitorInput::parse(r#"{"command":"true","timeout_ms":1800001}"#)
            .unwrap()
            .timeout_ms,
        1_800_001
    );
}

#[test]
fn duplicate_properties_use_last_value_without_changing_alias_precedence() {
    let exec = ExecInput::parse(r#"{"command":"first","command":null,"cmd":"alias","tty":true,"tty":false,"yield_after_ms":1,"yield_after_ms":2}"#).unwrap();
    assert_eq!(exec.command, "alias");
    assert!(!exec.tty);
    assert_eq!(exec.yield_after_ms, Some(2));
    let write =
        WriteStdinInput::parse(r#"{"name":"first","name":"last","input":"first","input":"last"}"#)
            .unwrap();
    assert_eq!(write.name, "last");
    assert_eq!(write.input, "last");
    assert_eq!(
        InterruptInput::parse(r#"{"name":"p","signal":1,"signal":64}"#)
            .unwrap()
            .signal,
        64
    );
    let monitor = MonitorInput::parse(r#"{"command":"first","command":"last","description":"first","description":"last","timeout_ms":1000,"timeout_ms":1800000}"#).unwrap();
    assert_eq!(monitor.command, "last");
    assert_eq!(monitor.description.as_deref(), Some("last"));
    assert_eq!(monitor.timeout_ms, 1_800_000);
}

#[test]
fn invalid_earlier_duplicates_are_not_hidden_by_valid_replacements() {
    assert_eq!(
        error(ExecInput::parse(r#"{"command":7,"command":"ok"}"#)),
        "Tool argument 'command' must be a string or null."
    );
    for members in [
        r#""command":7,"command":"ok""#,
        r#""cmd":7,"cmd":"ok""#,
        r#""name":7,"name":"p""#,
        r#""description":7,"description":"d""#,
        r#""yield_after_ms":1.0,"yield_after_ms":2"#,
        r#""tty":null,"tty":true"#,
        r#""env":[],"env":{}"#,
    ] {
        assert!(ExecInput::parse(&format!(r#"{{"command":"ok",{members}}}"#)).is_err());
    }
    for members in [
        r#""command":7,"command":"ok""#,
        r#""description":7,"description":"d""#,
        r#""name":7,"name":"p""#,
        r#""timeout_ms":1e3,"timeout_ms":1000"#,
        r#""env":[],"env":{}"#,
    ] {
        assert!(MonitorInput::parse(&format!(r#"{{"command":"ok",{members}}}"#)).is_err());
    }
    for members in [
        r#""name":7,"name":"p""#,
        r#""input":7,"input":"text""#,
        r#""yield_after_ms":1.0,"yield_after_ms":2"#,
    ] {
        assert!(
            WriteStdinInput::parse(&format!(r#"{{"name":"p","input":"",{members}}}"#)).is_err()
        );
    }
    for members in [
        r#""name":7,"name":"p""#,
        r#""signal":1.0,"signal":2"#,
        r#""signal":2147483648,"signal":2"#,
        r#""signal":"SIGINT","signal":null"#,
    ] {
        assert!(InterruptInput::parse(&format!(r#"{{"name":"p",{members}}}"#)).is_err());
    }
}

#[test]
fn every_environment_value_is_type_checked_before_duplicate_overwrite() {
    for members in [
        r#""env":{"VALUE":7,"VALUE":"ok"}"#,
        r#""env":{"VALUE":null},"env":{"VALUE":"ok"}"#,
    ] {
        let expected = "Tool argument 'env' must contain only string values.";
        assert_eq!(
            error(ExecInput::parse(&format!(
                r#"{{"command":"ok",{members}}}"#
            ))),
            expected
        );
        assert_eq!(
            error(MonitorInput::parse(&format!(
                r#"{{"command":"ok",{members}}}"#
            ))),
            expected
        );
    }
}

#[test]
fn environment_identifier_and_nul_validation_only_checks_final_dictionary() {
    for members in [
        r#""env":{"VALUE":"bad\u0000value","VALUE":"ok"}"#,
        r#""env":{"INVALID=NAME":"value"},"env":{"VALUE":"ok"}"#,
        r#""env":{"":"value"},"env":{"VALUE":"ok"}"#,
        r#""env":{"NUL\u0000NAME":"value"},"env":{"VALUE":"ok"}"#,
        r#""env":{"VALUE":"bad\u0000value"},"env":{"VALUE":"ok"}"#,
    ] {
        assert_eq!(
            ExecInput::parse(&format!(r#"{{"command":"ok",{members}}}"#))
                .unwrap()
                .environment["VALUE"],
            "ok"
        );
        assert_eq!(
            MonitorInput::parse(&format!(r#"{{"command":"ok",{members}}}"#))
                .unwrap()
                .environment["VALUE"],
            "ok"
        );
    }
    assert!(
        ExecInput::parse(r#"{"command":"ok","env":{"":"value"},"env":null}"#)
            .unwrap()
            .environment
            .is_empty()
    );
}

#[test]
fn semantic_bounds_apply_only_after_all_valid_typed_assignments() {
    assert_eq!(
        ExecInput::parse(r#"{"command":"ok","yield_after_ms":-1,"yield_after_ms":2}"#)
            .unwrap()
            .yield_after_ms,
        Some(2)
    );
    assert_eq!(
        WriteStdinInput::parse(r#"{"name":"p","input":"","yield_after_ms":-1,"yield_after_ms":2}"#)
            .unwrap()
            .yield_after_ms,
        2
    );
    assert_eq!(
        InterruptInput::parse(r#"{"name":"p","signal":65,"signal":2}"#)
            .unwrap()
            .signal,
        2
    );
    assert_eq!(
        InterruptInput::parse(r#"{"name":"p","signal":65,"signal":null}"#)
            .unwrap()
            .signal,
        2
    );
}

#[test]
fn typed_errors_and_unknown_properties_follow_encounter_order() {
    let unknown_first = r#"{"unexpected":true,"command":7}"#;
    let type_first = r#"{"command":7,"unexpected":true}"#;
    let unknown_error = "Tool argument 'unexpected' is not supported.";
    let type_error = "Tool argument 'command' must be a string or null.";
    assert_eq!(error(ExecInput::parse(unknown_first)), unknown_error);
    assert_eq!(error(MonitorInput::parse(unknown_first)), unknown_error);
    assert_eq!(error(ExecInput::parse(type_first)), type_error);
    assert_eq!(error(MonitorInput::parse(type_first)), type_error);
    assert_eq!(
        error(WriteStdinInput::parse(r#"{"input":7,"name":7}"#)),
        "Tool argument 'input' must be a string or null."
    );
    assert_eq!(
        error(WriteStdinInput::parse(r#"{"name":7,"input":7}"#)),
        "Tool argument 'name' must be a string or null."
    );
}

#[test]
fn interrupt_final_signal_precheck_precedes_typed_member_validation() {
    assert_eq!(
        error(InterruptInput::parse(
            r#"{"name":7,"signal":2,"signal":1.0}"#
        )),
        "Tool argument 'signal' must be an integer."
    );
    assert_eq!(
        error(InterruptInput::parse(
            r#"{"name":7,"signal":1.0,"signal":2}"#
        )),
        "Tool argument 'name' must be a string or null."
    );
    assert_eq!(
        error(InterruptInput::parse(
            r#"{"signal":1.0,"name":7,"signal":2}"#
        )),
        "Tool argument 'signal' must be an integer."
    );
}

#[test]
fn malformed_json_is_rejected() {
    for json in [
        "",
        "{",
        r#"{"command":"true",}"#,
        r#"{"command":"true"} {}"#,
        r#"{"command":"true","yield_after_ms":+1}"#,
    ] {
        assert!(ExecInput::parse(json).is_err());
    }
}
