use super::support::*;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_tools::session::{Failure, FailureKind, Kind};
use std::{future::Future, future::poll_fn, sync::Arc, task::Poll};

#[tokio::test]
async fn missing_reminder_awaits_titles_after_clear_and_preserves_their_order() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    {
        let mut state = fixture.services.0.lock().unwrap();
        state.reminders = vec![
            ("tests".into(), "run tests".into()),
            ("docs".into(), "update docs".into()),
        ];
        state.reminder_titles_gate = Some(gate.clone());
    }
    let mut executing =
        std::pin::pin!(fixture.invoke(Kind::ClearExitReminder, r#"{"title":"missing"}"#));

    assert!(
        poll_fn(|cx| Poll::Ready(executing.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(
        fixture.calls(),
        [Call::Clear("missing".into()), Call::Titles]
    );
    assert_eq!(fixture.services.0.lock().unwrap().reminders.len(), 2);

    gate.notify_one();
    assert_eq!(
        text(executing.await),
        "error: No exit reminder titled missing. Active titles: tests, docs"
    );
    assert_eq!(
        fixture.calls(),
        [Call::Clear("missing".into()), Call::Titles]
    );
}

#[tokio::test]
async fn reminders_set_replace_clear_with_exact_descriptions_and_ordered_missing_titles() {
    let fixture = Fixture::new();
    let steps = [
        (
            Kind::SetExitReminder,
            r#"{"title":"tests","description":"run tests"}"#,
            "Exit reminder set: tests: run tests",
        ),
        (
            Kind::SetExitReminder,
            r#"{"title":"docs","description":"  update {docs}  "}"#,
            "Exit reminder set: docs:   update {docs}  ",
        ),
        (
            Kind::SetExitReminder,
            r#"{"title":"tests","description":"run all tests"}"#,
            "Exit reminder set: tests: run all tests",
        ),
        (
            Kind::ClearExitReminder,
            r#"{"title":"missing"}"#,
            "error: No exit reminder titled missing. Active titles: tests, docs",
        ),
        (
            Kind::ClearExitReminder,
            r#"{"title":"tests"}"#,
            "Exit reminder cleared: tests",
        ),
        (
            Kind::ClearExitReminder,
            r#"{"title":"missing"}"#,
            "error: No exit reminder titled missing. Active titles: docs",
        ),
        (
            Kind::ClearExitReminder,
            r#"{"title":"docs"}"#,
            "Exit reminder cleared: docs",
        ),
        (
            Kind::ClearExitReminder,
            r#"{"title":"docs"}"#,
            "error: No exit reminder titled docs. Active titles: ",
        ),
    ];
    for (index, (kind, json, expected)) in steps.into_iter().enumerate() {
        assert_eq!(text(fixture.invoke(kind, json).await), expected);
        let expected_reminders: Vec<(String, String)> = match index {
            0 => vec![("tests".into(), "run tests".into())],
            1 => vec![
                ("tests".into(), "run tests".into()),
                ("docs".into(), "  update {docs}  ".into()),
            ],
            2 | 3 => vec![
                ("tests".into(), "run all tests".into()),
                ("docs".into(), "  update {docs}  ".into()),
            ],
            4 | 5 => vec![("docs".into(), "  update {docs}  ".into())],
            _ => vec![],
        };
        assert_eq!(
            fixture.services.0.lock().unwrap().reminders,
            expected_reminders
        );
    }
    assert_eq!(
        fixture.calls(),
        [
            Call::Set("tests".into(), "run tests".into()),
            Call::Set("docs".into(), "  update {docs}  ".into()),
            Call::Set("tests".into(), "run all tests".into()),
            Call::Clear("missing".into()),
            Call::Titles,
            Call::Clear("tests".into()),
            Call::Clear("missing".into()),
            Call::Titles,
            Call::Clear("docs".into()),
            Call::Clear("docs".into()),
            Call::Titles,
        ]
    );
}

#[tokio::test]
async fn whitespace_is_nonempty_and_optional_invocation_formatters_wrap_reminder_outputs() {
    let fixture = Fixture::new();
    let formatter = formatter_catalog();
    let title = " \t ";
    let description = " \n {description} ";
    let json = serde_json::json!({"title": title, "description": description}).to_string();
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::SetExitReminder,
                    &json,
                    Some(formatter.clone()),
                    &Cancel(false)
                )
                .await
        ),
        format!("TEXT<Exit reminder set: {title}: {description}>")
    );
    assert_eq!(
        fixture.services.0.lock().unwrap().reminders,
        [(title.to_owned(), description.to_owned())]
    );
    let json = serde_json::json!({"title": title}).to_string();
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::ClearExitReminder,
                    &json,
                    Some(formatter.clone()),
                    &Cancel(false)
                )
                .await
        ),
        format!("TEXT<Exit reminder cleared: {title}>")
    );
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::ClearExitReminder,
                    &json,
                    Some(formatter),
                    &Cancel(false)
                )
                .await
        ),
        format!("ERROR<No exit reminder titled {title}. Active titles: >")
    );
    assert!(fixture.services.0.lock().unwrap().reminders.is_empty());
}

#[tokio::test]
async fn invalid_reminder_arguments_have_no_mutation_or_service_calls() {
    let fixture = Fixture::new();
    for (kind, inputs) in [
        (
            Kind::SetExitReminder,
            vec![
                "null",
                "{}",
                "{",
                "[]",
                r#"{"title":"tests"}"#,
                r#"{"description":"run"}"#,
                r#"{"title":null,"description":"run"}"#,
                r#"{"title":"tests","description":null}"#,
                r#"{"title":"","description":"run"}"#,
                r#"{"title":"tests","description":""}"#,
                r#"{"title":1,"description":"run"}"#,
                r#"{"title":"tests","description":1}"#,
                r#"{"title":"tests","description":"run","extra":true}"#,
            ],
        ),
        (
            Kind::ClearExitReminder,
            vec![
                "null",
                "{}",
                "{",
                "[]",
                r#"{"title":null}"#,
                r#"{"title":""}"#,
                r#"{"title":1}"#,
                r#"{"title":"tests","extra":true}"#,
            ],
        ),
    ] {
        for json in inputs {
            assert!(
                text(fixture.invoke(kind, json).await).starts_with("error:"),
                "{kind:?}: {json}"
            );
        }
    }
    assert!(fixture.calls().is_empty());
    assert!(fixture.services.0.lock().unwrap().reminders.is_empty());
    assert_eq!(
        text(fixture.invoke(Kind::SetExitReminder, "null").await),
        "error: set_exit_reminder requires a nonempty title and description."
    );
    assert_eq!(
        text(fixture.invoke(Kind::ClearExitReminder, "null").await),
        "error: clear_exit_reminder requires a nonempty title."
    );
}

#[tokio::test]
async fn reminder_cancellation_and_service_errors_never_fabricate_success_or_mutation() {
    let fixture = Fixture::new();
    fixture
        .services
        .0
        .lock()
        .unwrap()
        .reminders
        .push(("keep".into(), "original".into()));
    for (kind, stage, json, call) in [
        (
            Kind::SetExitReminder,
            Stage::Set,
            r#"{"title":"keep","description":"replacement"}"#,
            Call::Set("keep".into(), "replacement".into()),
        ),
        (
            Kind::ClearExitReminder,
            Stage::Clear,
            r#"{"title":"keep"}"#,
            Call::Clear("keep".into()),
        ),
    ] {
        fixture.services.0.lock().unwrap().calls.clear();
        assert_eq!(
            fixture
                .invoke_with(kind, json, Some(formatter_catalog()), &Cancel(true))
                .await,
            Err(OperationError::Cancelled)
        );
        assert_eq!(fixture.calls(), [call.clone()]);
        for failure_kind in [
            FailureKind::Argument,
            FailureKind::InputConflict,
            FailureKind::Io,
        ] {
            let failure = Failure::new(failure_kind, "reminder failed");
            fixture.services.0.lock().unwrap().failure = Some((stage, failure.clone()));
            let result = fixture
                .invoke_with(kind, json, Some(formatter_catalog()), &Cancel(false))
                .await;
            if failure_kind == FailureKind::Io {
                assert_eq!(result, Err(OperationError::Failed(failure)));
            } else {
                assert_eq!(text(result), "ERROR<reminder failed>");
            }
            assert_eq!(
                fixture.services.0.lock().unwrap().reminders,
                [("keep".to_owned(), "original".to_owned())]
            );
        }
        let mut state = fixture.services.0.lock().unwrap();
        assert_eq!(state.calls, vec![call; 4]);
        state.failure = None;
    }
}
