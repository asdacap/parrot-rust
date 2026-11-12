use super::support::*;
use parrot_domain::{
    contracts::async_ops::OperationError,
    values::progress::{Progress, ProgressUpdate},
};
use parrot_tools::session::{Failure, FailureKind, Kind};

#[tokio::test]
async fn progress_replaces_counts_and_label_until_explicit_close_and_can_reopen() {
    let fixture = Fixture::new();
    for (json, expected, progress) in [
        (
            r#"{"steps":2,"total_steps":5,"current_step":"Running tests"}"#,
            "Progress updated: 2/5.",
            Some(Progress {
                steps: 2,
                total_steps: 5,
                current_step: Some("Running tests".into()),
            }),
        ),
        (
            r#"{"steps":5,"total_steps":5,"current_step":"Still open"}"#,
            "Progress updated: 5/5.",
            Some(Progress {
                steps: 5,
                total_steps: 5,
                current_step: Some("Still open".into()),
            }),
        ),
        (
            r#"{"steps":1,"total_steps":3}"#,
            "Progress updated: 1/3.",
            Some(Progress {
                steps: 1,
                total_steps: 3,
                current_step: None,
            }),
        ),
        (r#"{"close":true}"#, "Progress closed.", None),
        (r#"{"close":true}"#, "Progress is already closed.", None),
        (
            r#"{"steps":0,"total_steps":1,"current_step":""}"#,
            "Progress updated: 0/1.",
            Some(Progress {
                steps: 0,
                total_steps: 1,
                current_step: Some(String::new()),
            }),
        ),
    ] {
        assert_eq!(text(fixture.invoke(Kind::Progress, json).await), expected);
        assert_eq!(
            fixture.services.0.lock().unwrap().progress.progress,
            progress
        );
    }
    assert_eq!(fixture.services.0.lock().unwrap().progress.revision, 5);
    assert_eq!(fixture.calls().len(), 6);
    assert!(matches!(
        fixture.calls()[3],
        Call::Progress(ProgressUpdate::Close)
    ));
    assert!(matches!(
        fixture.calls()[4],
        Call::Progress(ProgressUpdate::Close)
    ));
}

#[tokio::test]
async fn progress_rejects_invalid_arguments_before_service_mutations() {
    let fixture = Fixture::new();
    for json in [
        "null",
        "[]",
        "{}",
        r#"{"steps":1}"#,
        r#"{"total_steps":3}"#,
        r#"{"current_step":"label"}"#,
        r#"{"steps":0,"total_steps":0}"#,
        r#"{"steps":4,"total_steps":3}"#,
        r#"{"steps":-1,"total_steps":3}"#,
        r#"{"steps":1.5,"total_steps":3}"#,
        r#"{"steps":1,"total_steps":3.0}"#,
        r#"{"steps":"1","total_steps":3}"#,
        r#"{"steps":null,"total_steps":3}"#,
        r#"{"steps":1,"total_steps":3,"current_step":true}"#,
        r#"{"steps":1,"total_steps":3,"current_step":null}"#,
        r#"{"steps":1,"total_steps":3,"close":"true"}"#,
        r#"{"steps":1,"total_steps":3,"close":null}"#,
        r#"{"steps":1,"total_steps":3,"name":"other-agent"}"#,
        r#"{"steps":18446744073709551616,"total_steps":3}"#,
        r#"{"steps":1,"steps":2,"total_steps":3}"#,
        r#"{"close":true,"steps":1}"#,
        r#"{"close":true,"total_steps":3}"#,
        r#"{"close":true,"steps":3,"total_steps":2}"#,
        r#"{"close":true,"steps":0,"total_steps":0}"#,
        r#"{"close":true,"steps":null,"total_steps":null}"#,
    ] {
        let result = text(fixture.invoke(Kind::Progress, json).await);
        assert!(result.starts_with("error:"), "{json}: {result}");
    }
    assert!(fixture.calls().is_empty());
    assert_eq!(fixture.services.0.lock().unwrap().progress.revision, 0);
}

#[tokio::test]
async fn close_validates_redundant_count_pairs_but_does_not_set_them() {
    let fixture = Fixture::new();
    for json in [
        r#"{"close":true,"steps":1}"#,
        r#"{"close":true,"total_steps":3}"#,
    ] {
        let result = text(fixture.invoke(Kind::Progress, json).await);
        assert!(result.contains("Closing may omit both counts, but cannot supply only one."));
    }
    assert!(fixture.calls().is_empty());
    assert_eq!(
        text(fixture.invoke(Kind::Progress, r#"{"close":true}"#).await),
        "Progress is already closed."
    );
    fixture
        .invoke(Kind::Progress, r#"{"steps":0,"total_steps":2}"#)
        .await
        .unwrap();
    assert_eq!(
        text(
            fixture
                .invoke(
                    Kind::Progress,
                    r#"{"steps":1,"total_steps":3,"current_step":"ignored","close":true}"#
                )
                .await
        ),
        "Progress closed."
    );
    assert_eq!(
        fixture.calls().last(),
        Some(&Call::Progress(ProgressUpdate::Close))
    );
    assert_eq!(fixture.services.0.lock().unwrap().progress.progress, None);
}

#[tokio::test]
async fn progress_preserves_u64_counts_and_uses_configurable_results_and_formatters() {
    let fixture = Fixture {
        templates: catalog(
            "prompt_templates:\n  progress-tool.updated:\n    template: 'COUNT<{steps}|{total_steps}>'\n  progress-tool.closed:\n    template: 'CLOSED'\n  progress-tool.already-closed:\n    template: 'ALREADY'\n",
        ),
        ..Fixture::new()
    };
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::Progress,
                    r#"{"steps":18446744073709551614,"total_steps":18446744073709551615}"#,
                    catalog(
                        "prompt_templates:\n  progress-tool.updated:\n    template: 'COUNT<{steps}|{total_steps}>'\n  tool-result.text:\n    template: 'TEXT<{value}>'\n",
                    ),
                    &Cancel(false)
                )
                .await
        ),
        "TEXT<COUNT<18446744073709551614|18446744073709551615>>"
    );
    assert_eq!(
        fixture
            .services
            .0
            .lock()
            .unwrap()
            .progress
            .progress
            .as_ref()
            .unwrap()
            .total_steps,
        u64::MAX
    );
    assert_eq!(
        text(fixture.invoke(Kind::Progress, r#"{"close":true}"#).await),
        "CLOSED"
    );
    assert_eq!(
        text(fixture.invoke(Kind::Progress, r#"{"close":true}"#).await),
        "ALREADY"
    );
}

#[tokio::test]
async fn progress_preserves_service_errors_and_cancellation_without_changing_state() {
    let fixture = Fixture::new();
    assert!(matches!(
        fixture
            .invoke_with(
                Kind::Progress,
                r#"{"steps":0,"total_steps":2}"#,
                fixture.templates.clone(),
                &Cancel(true)
            )
            .await,
        Err(OperationError::Cancelled)
    ));
    assert_eq!(fixture.services.0.lock().unwrap().progress.progress, None);
    for kind in [FailureKind::Io, FailureKind::Argument] {
        let failure = Failure::new(kind, "persist failed");
        fixture.services.0.lock().unwrap().failure = Some((Stage::Progress, failure.clone()));
        let result = fixture
            .invoke_with(
                Kind::Progress,
                r#"{"steps":1,"total_steps":2}"#,
                formatter_catalog(),
                &Cancel(false),
            )
            .await;
        if kind == FailureKind::Argument {
            assert_eq!(text(result), "ERROR<persist failed>");
        } else {
            assert_eq!(result, Err(OperationError::Failed(failure)));
        }
    }
    assert_eq!(fixture.services.0.lock().unwrap().progress.progress, None);
}

#[test]
fn progress_reminder_uses_scalar_arguments_and_partial_overrides() {
    let templates = catalog("");
    for current_step in ["", "Running tests"] {
        let output = templates
            .render(
                "agent-session.progress-reminder",
                &[
                    ("steps".into(), "2".into()),
                    ("total_steps".into(), "5".into()),
                    ("current_step".into(), current_step.into()),
                ],
                &Cancel(false),
            )
            .unwrap();
        assert!(output.contains("2/5"));
        assert!(output.contains("close: true"));
        assert_eq!(output.contains("Running tests"), !current_step.is_empty());
    }
    let templates = catalog(
        "prompt_templates:\n  agent-session.progress-reminder:\n    template: 'CUSTOM {steps}/{total_steps}: {current_step}'\n",
    );
    assert_eq!(
        templates
            .render(
                "agent-session.progress-reminder",
                &[
                    ("steps".into(), "2".into()),
                    ("total_steps".into(), "5".into()),
                    ("current_step".into(), "Running tests".into()),
                ],
                &Cancel(false),
            )
            .unwrap(),
        "CUSTOM 2/5: Running tests"
    );
}
