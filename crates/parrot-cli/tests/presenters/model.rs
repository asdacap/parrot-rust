use std::sync::Arc;

use parrot_cli::presenters::*;
use parrot_domain::protocol::messages::parrot::{ArtifactReference, YieldedShellProcess};

#[test]
fn redactor_conceals_non_string_fields_invalid_json_and_non_objects() {
    let metadata = ToolPresentationMetadata {
        redacted_input_fields: vec!["input".into()],
        ..Default::default()
    };
    let structured = redact_call(
        &ToolCallPresentation::new("sensitive", r#"{"input":{"secret":true},"safe":42}"#),
        &metadata,
    );
    assert_eq!(
        structured.arguments_json,
        r#"{"input":"<redacted>","safe":42}"#
    );
    for invalid in [
        "not-json secret",
        "{\"input\":\"secret",
        "[\"secret\"]",
        "null",
        "42",
    ] {
        assert_eq!(
            redact_call(&ToolCallPresentation::new("sensitive", invalid), &metadata).arguments_json,
            "<redacted>"
        );
    }
}

#[test]
fn redactor_counts_unicode_scalars_and_preserves_unaffected_member_order() {
    let metadata = ToolPresentationMetadata {
        redacted_input_fields: vec!["input".into()],
        ..Default::default()
    };
    let call = ToolCallPresentation::new(
        "sensitive",
        r#"{"z":1,"input":"secret \uD83D\uDD12","a":{"z":2,"a":3},"exponent":1e+2}"#,
    );
    let redacted = redact_call(&call, &metadata);
    assert_eq!(
        redacted.arguments_json,
        r#"{"z":1,"input":"<redacted: 8 chars>","a":{"z":2,"a":3},"exponent":1e+2}"#
    );
    assert!(!redacted.arguments_json.contains("secret"));
    assert!(call.arguments_json.contains("secret"));
    assert_eq!(
        redact_call(
            &ToolCallPresentation::new("sensitive", r#"{"input":"é😀"}"#),
            &metadata
        )
        .arguments_json,
        r#"{"input":"<redacted: 3 chars>"}"#
    );
}

#[test]
fn redactor_retains_arbitrary_precision_safe_members_while_concealing_sensitive_input() {
    let metadata = ToolPresentationMetadata {
        redacted_input_fields: vec!["input".into()],
        ..Default::default()
    };
    let call = ToolCallPresentation::new(
        "sensitive",
        r#"{"input":"secret","big":1234567890123456789012345678901234567890,"nested":{"exponent":1e999,"negative":-1e-999}}"#,
    );
    let redacted = redact_call(&call, &metadata);
    assert_eq!(
        redacted.arguments_json,
        r#"{"input":"<redacted: 6 chars>","big":1234567890123456789012345678901234567890,"nested":{"exponent":1e999,"negative":-1e-999}}"#
    );
    assert!(!redacted.arguments_json.contains("secret"));
}

#[test]
fn redactor_leaves_unselected_input_byte_identical_and_preserves_resolver() {
    let mut call = ToolCallPresentation::new("agent_send", " { \"name\": \"child\" } ");
    call.agent_reference_resolver = Arc::new(|name| format!("worker/{name}"));
    let unchanged = redact_call(&call, &ToolPresentationMetadata::default());
    assert_eq!(unchanged.arguments_json, call.arguments_json);
    assert_eq!(unchanged.resolve_agent_reference("child"), "worker/child");
}

#[test]
fn terminal_redaction_suppresses_all_text_but_retains_process_and_artifacts() {
    let terminal = ToolTerminalPresentation {
        status: ToolTerminalStatus::Errored,
        result_present: true,
        result: "private result".into(),
        error: "private error".into(),
        yielded_process: Some(YieldedShellProcess {
            process_id: "process-1".into(),
            ..Default::default()
        }),
        artifacts: vec![ArtifactReference {
            artifact_id: "artifact-1".into(),
            ..Default::default()
        }],
        start_omitted: true,
    };
    let metadata = ToolPresentationMetadata {
        suppress_terminal_details: true,
        ..Default::default()
    };
    let redacted = redact_terminal(&terminal, &metadata);
    assert!(!redacted.result_present);
    assert!(redacted.result.is_empty() && redacted.error.is_empty());
    assert_eq!(redacted.status, ToolTerminalStatus::Errored);
    assert_eq!(redacted.yielded_process.unwrap().process_id, "process-1");
    assert_eq!(redacted.artifacts[0].artifact_id, "artifact-1");
    assert!(redacted.start_omitted);
    assert_eq!(terminal.result, "private result");
}

#[test]
fn terminal_status_resolves_reported_error_without_broad_prefix_matching() {
    for (result, expected) in [
        (
            "error: operation failed",
            ToolTerminalStatus::ReportedFailure,
        ),
        ("error:", ToolTerminalStatus::Succeeded),
        ("Error: failed", ToolTerminalStatus::Succeeded),
        (" error: failed", ToolTerminalStatus::Succeeded),
    ] {
        let terminal = ToolTerminalPresentation {
            result_present: true,
            result: result.into(),
            ..Default::default()
        };
        assert_eq!(terminal.resolve_status(), expected, "{result}");
    }
    let absent = ToolTerminalPresentation {
        result_present: false,
        result: "error: hidden".into(),
        ..Default::default()
    };
    assert_eq!(absent.resolve_status(), ToolTerminalStatus::Succeeded);
}

#[test]
fn process_exit_status_accepts_exact_signed_int32_and_source_suffixes() {
    for result in [
        "Process exited with code 1",
        "Process exited with code -1\n[stderr]",
        "Process exited with code +2 after 0.1s",
        "Process exited with code -2147483648",
    ] {
        let terminal = ToolTerminalPresentation {
            result_present: true,
            result: result.into(),
            ..Default::default()
        };
        assert_eq!(
            terminal.resolve_process_status(),
            ToolTerminalStatus::ReportedFailure,
            "{result}"
        );
    }
    for result in [
        "Process exited with code 0",
        "Process exited with code -0 after 1s",
        "Process exited with code 1 later",
        "Process exited with code 1\r\n",
        "Process exited with code 2147483648",
        "Process exited with code 1.0",
        "Process exited with code ",
    ] {
        let terminal = ToolTerminalPresentation {
            result_present: true,
            result: result.into(),
            ..Default::default()
        };
        assert_eq!(
            terminal.resolve_process_status(),
            ToolTerminalStatus::Succeeded,
            "{result}"
        );
    }
}

#[test]
fn terminal_block_chooses_result_over_error_and_preserves_success_kind_rules() {
    let errored = ToolTerminalPresentation {
        status: ToolTerminalStatus::Errored,
        result_present: true,
        result: "selected result".into(),
        error: "unselected error".into(),
        ..Default::default()
    };
    assert_eq!(
        errored.describe_block(ToolBlockKind::None),
        ToolBlock::from_error("selected result")
    );
    let absent = ToolTerminalPresentation {
        result_present: false,
        ..errored.clone()
    };
    assert_eq!(
        absent.describe_block(ToolBlockKind::Text),
        ToolBlock::from_error("unselected error")
    );
    let success = ToolTerminalPresentation {
        status: ToolTerminalStatus::Succeeded,
        ..errored
    };
    assert_eq!(
        success.describe_block(ToolBlockKind::Text),
        ToolBlock::from_text("selected result")
    );
    assert_eq!(
        success.describe_block(ToolBlockKind::Diff),
        ToolBlock::from_diff("selected result")
    );
    assert_eq!(
        success.describe_block(ToolBlockKind::Output),
        ToolBlock::empty()
    );
}

#[test]
fn empty_diff_and_completed_input_retain_layout_while_empty_text_collapses() {
    for kind in [
        ToolBlockKind::Text,
        ToolBlockKind::Status,
        ToolBlockKind::Output,
        ToolBlockKind::Queue,
        ToolBlockKind::Error,
    ] {
        let empty = match kind {
            ToolBlockKind::Text => ToolBlock::from_text(""),
            ToolBlockKind::Status => ToolBlock::from_status(""),
            ToolBlockKind::Output => ToolBlock::from_output(""),
            ToolBlockKind::Queue => ToolBlock::from_queue(""),
            _ => ToolBlock::from_error(""),
        };
        assert_eq!(empty, ToolBlock::empty());
    }
    assert_eq!(ToolBlock::from_diff("").kind, ToolBlockKind::Diff);
    assert_eq!(
        ToolBlock::from_completed_input("").kind,
        ToolBlockKind::CompletedInput
    );
    assert_eq!(
        ToolBlock::from_details(["".into(), "a".into(), "".into(), "b".into()]),
        ToolBlock::from_text("a\nb")
    );
}
