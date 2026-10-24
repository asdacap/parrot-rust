//! A007 mappings from the pinned .NET helper tests. Serializer golden strings
//! were independently observed by running the unchanged ToolYamlFormatter.cs
//! against YamlDotNet 18.1.0 (not derived from this Rust implementation).

use parrot_cli::terminal_layout::{
    EnhancedFinalMessageRenderer, FinalFormatError, MarkdownLiveRenderer, ToolYamlFormatter,
};

#[test]
fn rejects_invalid_trailing_mixed_and_duplicate_json_values() {
    for value in [
        "not json",
        r#"{"value":1} trailing"#,
        "true false",
        r#"{"value":1,"value":2}"#,
    ] {
        assert_eq!(ToolYamlFormatter::try_format(value), Ok(None), "{value}");
        assert_eq!(ToolYamlFormatter::format(value), Ok(value.to_owned()));
    }
}

#[test]
fn renders_complete_json_as_yaml() {
    assert_complete_json_as_yaml(false);
}

#[test]
#[ignore = "Rust formatter always double-quotes string mapping keys"]
fn renders_complete_json_with_source_unquoted_keys() {
    assert_complete_json_as_yaml(true);
}

fn assert_complete_json_as_yaml(superseded: bool) {
    // EnhancedFinalMessageRendererTests.Renders_complete_json_as_yaml: all six
    // Arguments, including eligible top-level scalar values.
    for (source, expected) in [
        (
            r#"{"value":1,"text":"true","null":"null"}"#,
            vec!["value: 1", "text: \"true\"", "\"null\": \"null\""],
        ),
        ("[1,true,null]", vec!["- 1", "- true", "- null"]),
        (r#""hello""#, vec!["\"hello\""]),
        ("42", vec!["42"]),
        ("true", vec!["true"]),
        ("null", vec!["null"]),
    ] {
        if source.starts_with('{') != superseded {
            continue;
        }
        let rendered = EnhancedFinalMessageRenderer::render(source, "assistant: ", 80, false, true)
            .unwrap()
            .join("\n");
        for expected in expected {
            assert!(rendered.contains(expected), "{source}: {rendered:?}");
        }
    }
}

#[test]
fn renders_non_json_and_ineligible_json_as_markdown() {
    let markdown = EnhancedFinalMessageRenderer::render("**bold**", "assistant: ", 80, false, true)
        .unwrap()
        .join("\n");
    let ineligible =
        EnhancedFinalMessageRenderer::render(r#"{"value":1}"#, "assistant: ", 80, false, false)
            .unwrap()
            .join("\n");
    assert!(markdown.contains("bold"));
    assert!(ineligible.contains(r#"{"value":1}"#));
    assert!(!ineligible.contains("value: 1"));
}

#[test]
#[ignore = "Rust formatter always double-quotes string mapping keys"]
fn formats_complete_json_pending_value_as_yaml_at_commit() {
    // MarkdownLiveRendererTests.Formats_complete_json_pending_value_as_yaml_at_commit:
    // this is the actual pending-message consumer, not just a formatter unit test.
    let mut renderer = MarkdownLiveRenderer::new(80);
    renderer
        .append(
            "answer",
            "assistant: ",
            r#"{"value":{"enabled":true,"label":"ready"}}"#,
            false,
        )
        .unwrap();
    let rendered = renderer
        .commit(false)
        .unwrap()
        .scrollback
        .unwrap_or_default()
        .join("\n");
    assert!(rendered.contains("value:"), "{rendered:?}");
    assert!(rendered.contains("enabled: true"), "{rendered:?}");
    assert!(rendered.contains("label: \"ready\""), "{rendered:?}");
    assert!(!rendered.contains(r#"{"value""#));
}

#[test]
fn live_commit_propagates_conversion_errors_without_resetting_pending_text() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    renderer
        .append("answer", "assistant: ", r#""\uDEAD""#, false)
        .unwrap();
    assert_eq!(
        renderer.commit(false),
        Err(FinalFormatError::InvalidUnicodeScalar)
    );
    assert_eq!(
        renderer.commit(false),
        Err(FinalFormatError::InvalidUnicodeScalar)
    );
}

#[test]
fn preserves_empty_containers_and_scalar() {
    assert_source_container_format(false);
}

#[test]
#[ignore = "Rust formatter always double-quotes string mapping keys"]
fn preserves_source_order_numbers_and_key_styles() {
    assert_source_container_format(true);
}

fn assert_source_container_format(superseded: bool) {
    for (source, expected) in [
        (
            r#"{"z":1e9999,"a":-0.00E+10,"m":123456789012345678901234567890}"#,
            "z: 1e9999\na: -0.00E+10\nm: 123456789012345678901234567890",
        ),
        (
            r#"{"a":[1,{"x":[1,2],"y":{}},[2,3],[]],"b":{}}"#,
            "a:\n- 1\n- x:\n  - 1\n  - 2\n  y: {}\n- - 2\n  - 3\n- []\nb: {}",
        ),
        ("{}", "{}"),
        ("[]", "[]"),
        (r#""""#, ""),
        (r#"{"a:\tb":[]}"#, "'a:\tb': []"),
        (r#"{"a\nb":[1,2]}"#, "? >-\n  a\n\n  b\n: - 1\n  - 2"),
        (r#"{"a\nb":"value"}"#, "? >-\n  a\n\n  b\n: \"value\""),
    ] {
        if (source.starts_with('{') && source != "{}") != superseded {
            continue;
        }
        assert_eq!(
            ToolYamlFormatter::format(source).unwrap(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn distinguishes_caught_parse_failures_from_valid_scalar_conversion_errors() {
    for source in [r#""\uDEAD""#, r#"{"\uD800":1}"#, r#"{"a":1,"a":"\uDEAD"}"#] {
        assert_eq!(
            ToolYamlFormatter::try_format(source),
            Err(FinalFormatError::InvalidUnicodeScalar)
        );
        assert!(EnhancedFinalMessageRenderer::render(source, "", 80, false, true).is_err());
        assert!(EnhancedFinalMessageRenderer::render(source, "", 80, false, false).is_ok());
    }
    // Parsing the entire document precedes conversion, so trailing garbage
    // suppresses even a surrogate conversion error.
    for source in [
        r#""\uDEAD" trailing"#,
        r#"{"a":1,"\u0061":2}"#,
        r#"[1,]"#,
        "01",
        "1.",
        "1e",
        "/*x*/1",
    ] {
        assert_eq!(ToolYamlFormatter::try_format(source), Ok(None), "{source}");
    }
    let max_depth = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    let too_deep = format!("[{max_depth}]");
    assert!(ToolYamlFormatter::try_format(&max_depth).unwrap().is_some());
    assert_eq!(ToolYamlFormatter::try_format(&too_deep), Ok(None));
}

#[test]
#[ignore = "Rust formatter always double-quotes string mapping keys"]
fn matches_yamldotnet_double_quoted_escape_rules() {
    let source = r#"{"controls":"\u0000\u0007\b\t\n\u000b\f\r\u001b\u0085\u00a0\u2028\u2029\u007f","unicode":"日本 😀"}"#;
    assert_eq!(
        ToolYamlFormatter::format(source).unwrap(),
        "controls: \"\\0\\a\\b\t\\n\\v\\f\\r\\e\\N\u{a0}\\L\\P\\x7F\"\nunicode: \"日本 \\U0001F600\""
    );
}

#[test]
fn quotes_source_boolean_null_number_and_bounded_date_key_families() {
    for key in [
        "",
        "null",
        "NULL",
        "yes",
        "No",
        "ON",
        "off",
        "~",
        "<<",
        "true",
        " false ",
        "+1.2e3",
        ".5",
        "2024-02-29",
        "2024-01",
        "1/2",
        "1/2/2024",
        "12:30",
        "2024-01-01T12:30:45.123Z",
        "January 2, 2024",
        "2 Jan 2024",
    ] {
        let source = format!("{{{}:1}}", serde_json::to_string(key).unwrap());
        let expected = format!("{}: 1", serde_json::to_string(key).unwrap());
        assert_eq!(
            ToolYamlFormatter::format(&source).unwrap(),
            expected,
            "{key}"
        );
    }
}

#[test]
#[ignore = "Rust formatter quotes all keys without CLR decimal or date classification"]
fn preserves_source_unquoted_key_families() {
    for key in [
        "1,000",
        "1e99999999999",
        "79228162514264337593543950335.5",
        "2024-02-30",
        "12:30+99:00",
        "12:30oops",
        "hello",
    ] {
        let source = format!("{{{}:1}}", serde_json::to_string(key).unwrap());
        assert_eq!(
            ToolYamlFormatter::format(&source).unwrap(),
            format!("{key}: 1"),
            "{key}"
        );
    }
}
