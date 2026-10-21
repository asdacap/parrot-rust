use super::support::*;
use parrot_domain::{contracts::async_ops::OperationError, values::context::ContextSize};
use parrot_tools::session::{Compaction, Failure, FailureKind, Kind};

fn compacted(tokens: i64, usage: &str) -> String {
    format!(
        "Compacted this calling agent session. Context is now {usage}% used ({tokens} estimated tokens / 1000 limit); notifications occur every 10%; automatic compaction triggers strictly above 80%."
    )
}

#[tokio::test]
async fn optional_target_parsing_calls_compact_once_and_stable_estimate_once() {
    let fixture = Fixture::new();
    for (json, target) in [
        ("{}", None),
        (r#"{"target_context_size":null}"#, None),
        (
            r#"{"target_context_size":"20%"}"#,
            Some(ContextSize::parse("20%").unwrap()),
        ),
        (
            r#"{"target_context_size":"2k"}"#,
            Some(ContextSize::parse("2k").unwrap()),
        ),
    ] {
        fixture.services.0.lock().unwrap().calls.clear();
        let expected = compacted(250, "25");
        assert_eq!(
            text(
                fixture
                    .invoke_with(
                        Kind::CompactContext,
                        json,
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
                Call::Compact(SELECTION.into(), target),
                Call::Estimate(SELECTION.into(), CALL_ID.into(), expected),
            ]
        );
    }
}

#[tokio::test]
async fn invalid_compaction_input_never_compacts_or_estimates() {
    let fixture = Fixture::new();
    for json in [
        "",
        "{",
        "null",
        "[]",
        "1",
        r#"{"extra":true}"#,
        r#"{"target_context_size":""}"#,
        r#"{"target_context_size":" "}"#,
        r#"{"target_context_size":20}"#,
        r#"{"target_context_size":[]}"#,
        r#"{"target_context_size":"0"}"#,
        r#"{"target_context_size":"101%"}"#,
        r#"{"target_context_size":"invalid"}"#,
    ] {
        let output = text(
            fixture
                .invoke_with(
                    Kind::CompactContext,
                    json,
                    Some(formatter_catalog()),
                    &Cancel(false),
                )
                .await,
        );
        assert!(output.starts_with("ERROR<"), "{json}: {output}");
    }
    assert!(fixture.calls().is_empty());
    assert_eq!(
        text(
            fixture
                .invoke(Kind::CompactContext, r#"{"target_context_size":""}"#)
                .await
        ),
        "error: target_context_size must not be empty."
    );
}

#[tokio::test]
async fn convergence_estimates_each_current_result_with_captured_selection_and_call_id() {
    let fixture = Fixture::new();
    fixture.services.0.lock().unwrap().estimates.extend([
        Ok(snapshot(300, Some(30))),
        Ok(snapshot(400, Some(40))),
        Ok(snapshot(400, Some(40))),
    ]);
    assert_eq!(
        text(fixture.invoke(Kind::CompactContext, "{}").await),
        compacted(400, "40")
    );
    assert_eq!(
        fixture.calls(),
        [
            Call::Compact(SELECTION.into(), None),
            Call::Estimate(SELECTION.into(), CALL_ID.into(), compacted(250, "25")),
            Call::Estimate(SELECTION.into(), CALL_ID.into(), compacted(300, "30")),
            Call::Estimate(SELECTION.into(), CALL_ID.into(), compacted(400, "40")),
        ]
    );
    assert!(fixture.services.0.lock().unwrap().estimates.is_empty());
}

#[tokio::test]
async fn continually_changing_context_stops_after_eight_estimates_and_returns_latest_context() {
    let fixture = Fixture::new();
    fixture
        .services
        .0
        .lock()
        .unwrap()
        .estimates
        .extend((251..=258).map(|tokens| Ok(snapshot(tokens, Some(25)))));
    assert_eq!(
        text(fixture.invoke(Kind::CompactContext, "{}").await),
        compacted(258, "25")
    );
    let mut expected = vec![Call::Compact(SELECTION.into(), None)];
    expected.extend(
        (250..=257).map(|tokens| {
            Call::Estimate(SELECTION.into(), CALL_ID.into(), compacted(tokens, "25"))
        }),
    );
    assert_eq!(fixture.calls(), expected);
    assert!(fixture.services.0.lock().unwrap().estimates.is_empty());
}

#[tokio::test]
async fn predefined_compaction_templates_cover_reduced_noop_unavailable_and_limit_overrides() {
    let fixture = Fixture::new();
    let mut unavailable = snapshot(125, None);
    unavailable.context_limit = 0;
    unavailable.trigger_tokens = None;
    let mut with_limit = snapshot(250, None);
    with_limit.has_context_limit_override = true;
    with_limit.trigger_tokens = Some(600);
    let mut unavailable_with_limit = unavailable.clone();
    unavailable_with_limit.has_context_limit_override = true;
    for (reduced, context, expected) in [
        (true, snapshot(250, Some(25)), compacted(250, "25")),
        (false, snapshot(250, Some(25)), "No eligible context was compacted in this calling agent session. Context is 25% used (250 estimated tokens / 1000 limit); notifications occur every 10%; automatic compaction triggers strictly above 80%.".into()),
        (false, unavailable.clone(), "Context compaction is unavailable for this calling agent session because the selected model has no positive context limit (125 estimated tokens / 0 limit); notifications occur every 10%; automatic compaction triggers strictly above 80%.".into()),
        (true, with_limit.clone(), "Compacted this calling agent session. Context is now unavailable% used (250 estimated tokens / 1000 model context window); notifications occur every 10%; automatic compaction triggers strictly above 600 tokens.".into()),
        (false, with_limit, "No eligible context was compacted in this calling agent session. Context is unavailable% used (250 estimated tokens / 1000 model context window); notifications occur every 10%; automatic compaction triggers strictly above 600 tokens.".into()),
        (true, unavailable_with_limit, "Context compaction is unavailable for this calling agent session because the selected model has no positive context window (125 estimated tokens / 0 model context window); notifications occur every 10%; automatic compaction trigger: unavailable tokens.".into()),
        // Availability takes precedence over whether the service reports reduction.
        (true, unavailable, "Context compaction is unavailable for this calling agent session because the selected model has no positive context limit (125 estimated tokens / 0 limit); notifications occur every 10%; automatic compaction triggers strictly above 80%.".into()),
    ] {
        {
            let mut state = fixture.services.0.lock().unwrap();
            state.compaction = Compaction { reduced, context };
            state.calls.clear();
        }
        assert_eq!(text(fixture.invoke_with(Kind::CompactContext, "{}", Some(formatter_catalog()), &Cancel(false)).await), expected);
        assert_eq!(fixture.calls(), [Call::Compact(SELECTION.into(), None), Call::Estimate(SELECTION.into(), CALL_ID.into(), expected)]);
    }
}

#[tokio::test]
async fn injected_compaction_template_is_used_directly_with_all_source_arguments() {
    let mut fixture = Fixture::new();
    fixture.templates = catalog(
        "prompt_templates:\n  compact-context-tool.compacted-with-limit:\n    template: 'custom {estimated_tokens}|{context_limit}|{notification_interval}|{trigger}|{trigger_tokens}|{usage}'\n",
    );
    {
        let mut state = fixture.services.0.lock().unwrap();
        state.compaction.context.estimated_tokens = 987;
        state.compaction.context.context_limit = 4321;
        state.compaction.context.usage_percent = Some(17);
        state.compaction.context.trigger_percent = 73;
        state.compaction.context.has_context_limit_override = true;
        state.compaction.context.trigger_tokens = Some(555);
    }
    let expected = "custom 987|4321|10|73|555|17";
    assert_eq!(
        text(
            fixture
                .invoke_with(
                    Kind::CompactContext,
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
            Call::Compact(SELECTION.into(), None),
            Call::Estimate(SELECTION.into(), CALL_ID.into(), expected.into())
        ]
    );
}

#[tokio::test]
async fn compact_and_estimation_failures_and_cancellation_propagate_without_success_text() {
    let fixture = Fixture::new();
    for kind in [FailureKind::Argument, FailureKind::Io] {
        let failure = Failure::new(kind, "compact operation failed");
        {
            let mut state = fixture.services.0.lock().unwrap();
            state.failure = Some((Stage::Compact, failure.clone()));
            state.calls.clear();
        }
        assert_eq!(
            fixture
                .invoke_with(
                    Kind::CompactContext,
                    "{}",
                    Some(formatter_catalog()),
                    &Cancel(false)
                )
                .await,
            Err(OperationError::Failed(failure))
        );
        assert_eq!(fixture.calls(), [Call::Compact(SELECTION.into(), None)]);
    }
    {
        let mut state = fixture.services.0.lock().unwrap();
        state.failure = None;
        state.calls.clear();
    }
    assert_eq!(
        fixture
            .invoke_with(
                Kind::CompactContext,
                "{}",
                Some(formatter_catalog()),
                &Cancel(true)
            )
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(fixture.calls(), [Call::Compact(SELECTION.into(), None)]);
    let failure = Failure::new(FailureKind::Io, "estimate failed");
    {
        let mut state = fixture.services.0.lock().unwrap();
        state.calls.clear();
        state.estimates.push_back(Err(failure.clone()));
    }
    assert_eq!(
        fixture
            .invoke_with(
                Kind::CompactContext,
                "{}",
                Some(formatter_catalog()),
                &Cancel(false)
            )
            .await,
        Err(OperationError::Failed(failure))
    );
    assert_eq!(
        fixture.calls(),
        [
            Call::Compact(SELECTION.into(), None),
            Call::Estimate(SELECTION.into(), CALL_ID.into(), compacted(250, "25"))
        ]
    );
}
