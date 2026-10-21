use super::support::*;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CancelAtStatistics(AtomicUsize);
impl Cancellation for CancelAtStatistics {
    fn is_cancelled(&self) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst) >= 1
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
use parrot_tools::session::{Failure, FailureKind, Kind};

#[tokio::test]
async fn status_observes_runtime_then_statistics_and_joins_only_nonblank_sections_verbatim() {
    let fixture = Fixture::new();
    for (runtime, statistics, expected) in [
        (
            " runtime {detail}\n",
            " statistics ",
            " runtime {detail}\n\n\n statistics ",
        ),
        (" \n\t", "statistics", "statistics"),
        ("runtime", " \n\t", "runtime"),
    ] {
        {
            let mut state = fixture.services.0.lock().unwrap();
            state.runtime = runtime.into();
            state.statistics = statistics.into();
            state.calls.clear();
        }
        assert_eq!(
            text(
                fixture
                    .invoke_with(
                        Kind::Status,
                        "{}",
                        Some(formatter_catalog()),
                        &Cancel(false)
                    )
                    .await
            ),
            expected
        );
        assert_eq!(
            fixture.calls(),
            [
                Call::Runtime(SELECTION.into()),
                Call::Statistics(SELECTION.into())
            ]
        );
    }
}

#[tokio::test]
async fn unavailable_status_uses_optional_formatter_but_nonempty_status_is_direct() {
    let fixture = Fixture::new();
    fixture.services.0.lock().unwrap().runtime = " \t\n".into();
    assert_eq!(
        text(fixture.invoke(Kind::Status, "{}").await),
        "No runtime status is currently available."
    );
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::Status,
                    "{}",
                    Some(formatter_catalog()),
                    &Cancel(false)
                )
                .await
        ),
        "TEXT<No runtime status is currently available.>"
    );
    assert_eq!(
        fixture.calls(),
        [
            Call::Runtime(SELECTION.into()),
            Call::Statistics(SELECTION.into()),
            Call::Runtime(SELECTION.into()),
            Call::Statistics(SELECTION.into())
        ]
    );
}

#[tokio::test]
async fn status_accepts_only_empty_object_and_never_observes_invalid_input() {
    let fixture = Fixture::new();
    for json in ["", "{", "null", "[]", "1", r#""text""#, r#"{"extra":true}"#] {
        let output = text(
            fixture
                .invoke_with(
                    Kind::Status,
                    json,
                    Some(formatter_catalog()),
                    &Cancel(false),
                )
                .await,
        );
        assert!(output.starts_with("ERROR<"), "{json}: {output}");
    }
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn status_observation_failures_propagate_without_reformatting_and_short_circuit_in_order() {
    let fixture = Fixture::new();
    for stage in [Stage::Runtime, Stage::Statistics] {
        for kind in [FailureKind::Argument, FailureKind::Io] {
            let failure = Failure::new(kind, "status observation failed");
            {
                let mut state = fixture.services.0.lock().unwrap();
                state.failure = Some((stage, failure.clone()));
                state.calls.clear();
            }
            assert_eq!(
                fixture
                    .invoke_with(
                        Kind::Status,
                        "{}",
                        Some(formatter_catalog()),
                        &Cancel(false)
                    )
                    .await,
                Err(OperationError::Failed(failure))
            );
            let expected = if stage == Stage::Runtime {
                vec![Call::Runtime(SELECTION.into())]
            } else {
                vec![
                    Call::Runtime(SELECTION.into()),
                    Call::Statistics(SELECTION.into()),
                ]
            };
            assert_eq!(fixture.calls(), expected);
        }
    }
    fixture.services.0.lock().unwrap().calls.clear();
    assert_eq!(
        fixture
            .invoke_with(Kind::Status, "{}", Some(formatter_catalog()), &Cancel(true))
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(fixture.calls(), [Call::Runtime(SELECTION.into())]);
    {
        let mut state = fixture.services.0.lock().unwrap();
        state.failure = None;
        state.runtime = "partial runtime must not escape".into();
        state.calls.clear();
    }
    let cancellation = CancelAtStatistics(AtomicUsize::new(0));
    assert_eq!(
        fixture
            .invoke_with(Kind::Status, "{}", Some(formatter_catalog()), &cancellation)
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(
        fixture.calls(),
        [
            Call::Runtime(SELECTION.into()),
            Call::Statistics(SELECTION.into())
        ]
    );
}
