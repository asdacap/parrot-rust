use super::support::*;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_tools::session::{Failure, FailureKind, Kind};
use std::{future::Future, future::poll_fn, sync::Arc, task::Poll};

#[tokio::test]
async fn checkpoint_awaits_service_completion_before_returning_success() {
    let fixture = Fixture::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    fixture.services.0.lock().unwrap().checkpoint_gate = Some(gate.clone());
    let mut executing =
        std::pin::pin!(fixture.invoke(Kind::SetCheckpoint, r#"{"title":"handoff"}"#));

    assert!(
        poll_fn(|cx| Poll::Ready(executing.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(
        fixture.calls(),
        [Call::Checkpoint(
            "handoff".into(),
            ASSISTANT_SEQUENCE,
            CALL_ID.into()
        )]
    );
    assert!(fixture.services.0.lock().unwrap().checkpoints.is_empty());

    gate.notify_one();
    assert_eq!(text(executing.await), "handoff");
    assert_eq!(
        fixture.services.0.lock().unwrap().checkpoints,
        [("handoff".to_owned(), ASSISTANT_SEQUENCE, CALL_ID.to_owned())]
    );
}

#[tokio::test]
async fn checkpoint_forwards_verbatim_title_durable_sequence_and_call_id_without_text_formatter() {
    let fixture = Fixture::new();
    let title = "  handoff {title}\n😀  ";
    let json = serde_json::json!({"title": title}).to_string();
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::SetCheckpoint,
                    &json,
                    Some(formatter_catalog()),
                    &Cancel(false)
                )
                .await
        ),
        title
    );
    assert_eq!(
        fixture.calls(),
        [Call::Checkpoint(
            title.into(),
            ASSISTANT_SEQUENCE,
            CALL_ID.into()
        )]
    );
    assert_eq!(
        fixture.services.0.lock().unwrap().checkpoints,
        [(title.to_owned(), ASSISTANT_SEQUENCE, CALL_ID.to_owned())]
    );
}

#[tokio::test]
async fn checkpoint_argument_and_conflict_failures_are_formatted_but_io_propagates() {
    let fixture = Fixture::new();
    for kind in [
        FailureKind::Argument,
        FailureKind::InputConflict,
        FailureKind::Format,
        FailureKind::Json,
        FailureKind::Io,
    ] {
        let failure = Failure::new(kind, "unrecordable {checkpoint}\nverbatim");
        fixture.services.0.lock().unwrap().failure = Some((Stage::Checkpoint, failure.clone()));
        let result = fixture
            .invoke_with(
                Kind::SetCheckpoint,
                r#"{"title":"handoff"}"#,
                Some(formatter_catalog()),
                &Cancel(false),
            )
            .await;
        if kind == FailureKind::Io {
            assert_eq!(result, Err(OperationError::Failed(failure)));
        } else {
            assert_eq!(text(result), format!("ERROR<{}>", failure.message));
        }
    }
    let state = fixture.services.0.lock().unwrap();
    assert!(state.checkpoints.is_empty());
    assert_eq!(
        state.calls,
        vec![Call::Checkpoint("handoff".into(), ASSISTANT_SEQUENCE, CALL_ID.into()); 5]
    );
}

#[tokio::test]
async fn invalid_checkpoint_arguments_never_reach_checkpoint_service() {
    let fixture = Fixture::new();
    for json in [
        "",
        "{",
        "null",
        "[]",
        "1",
        "{}",
        r#"{"title":null}"#,
        r#"{"title":1}"#,
        r#"{"title":[]}"#,
        r#"{"title":"handoff","extra":true}"#,
    ] {
        assert!(
            text(fixture.invoke(Kind::SetCheckpoint, json).await).starts_with("error:"),
            "{json}"
        );
    }
    assert!(fixture.calls().is_empty());
    assert!(fixture.services.0.lock().unwrap().checkpoints.is_empty());
}
