//! T59 actual-runtime queue-through-Tool proof, not P08 production host composition.
//! Canonical source identities below are preserved from downstream-open-contracts.md:55–62.

use super::runtime_fixture::*;
use parrot_domain::contracts::{
    async_ops::OperationError,
    tools::{ParallelSafety, Tool},
};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::events::NeverCancelled;
use parrot_tools::tasks_queues::queues::QueueToolKind::{Create, Info, Push, Take};
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn all_queue_factories_preserve_actual_configured_schema_and_defaults() {
    use parrot_domain::values::tools::ToolDefinitionCatalog;
    use parrot_foundation::config::{
        configuration::source_predefined_content, runtime::read_tool_definitions, yaml,
    };
    use parrot_tools::tasks_queues::queues::QueueTool;
    use std::sync::Arc;
    let root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
    let definitions = ToolDefinitionCatalog::new(
        &read_tool_definitions(&root)
            .unwrap()
            .into_iter()
            .map(|(name, definition)| {
                (
                    name,
                    parrot_domain::values::tools::ConfiguredToolDefinition::new(
                        definition.description,
                        definition.parameters_json,
                    ),
                )
            })
            .collect(),
    );
    let fixture = Fixture::new().await;
    for kind in [Create, Info, Push, Take] {
        let tool: RuntimeTool = QueueTool::new(
            kind,
            &definitions,
            Arc::new(Backend::new(
                fixture.root.clone(),
                fixture.runtime.registry.clone(),
            )),
            Arc::new(UnavailableReader),
            fixture.clock.clone(),
            fixture.log.clone(),
        )
        .unwrap();
        assert_eq!(
            tool.definition(),
            definitions.describe(kind.name()).unwrap()
        );
        let parameters: serde_json::Value =
            serde_json::from_str(&tool.definition().parameters_json).unwrap();
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(parameters["required"], json!(["name"]));
        match kind {
            Create => assert_eq!(
                parameters["properties"]["name"]["pattern"],
                "^[a-z0-9]+(?:-[a-z0-9]+)*$"
            ),
            Info => assert_eq!(parameters["properties"].as_object().unwrap().len(), 1),
            Push => {
                assert_eq!(parameters["properties"]["direction"]["default"], "back");
                assert_eq!(parameters["properties"]["close"]["default"], false);
                assert!(parameters.get("oneOf").is_none());
            }
            Take => {
                assert_eq!(parameters["properties"]["direction"]["default"], "front");
                assert_eq!(parameters["properties"]["count"]["default"], 1);
                assert_eq!(
                    parameters["properties"]["yield_after_ms"]["default"],
                    30_000
                );
            }
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn null_root_is_the_source_object_error_before_any_mutation() {
    let fixture = Fixture::new().await;
    for kind in [Create, Info, Push, Take] {
        assert_eq!(
            fixture.call(&fixture.root, kind, "null").await.text,
            "error: Tool arguments must be an object."
        );
    }
    assert!(fixture.runtime.q(&fixture.root).list().unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn nullable_wire_items_reach_store_validation_after_resolution_and_closed_check() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    assert_eq!(
        f.call(
            &f.root,
            Push,
            r#"{"name":"work","items":[null],"close":true}"#
        )
        .await
        .text,
        "error: queue: items cannot contain null"
    );
    let state = f.runtime.q(&f.root).get("work").unwrap();
    assert_eq!(state.size, 0);
    assert!(!state.closed);
    assert_eq!(
        f.call(&f.root, Push, r#"{"name":"absent","items":[null]}"#)
            .await
            .text,
        "error: queue: 'absent' was not found"
    );
    f.call(&f.root, Push, r#"{"name":"work","items":[],"close":true}"#)
        .await;
    assert_eq!(
        f.call(&f.root, Push, r#"{"name":"work","items":[null]}"#)
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    assert_eq!(
        f.call(&f.root, Push, r#"{"name":"/root/work","items":[null]}"#)
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    f.close().await;
}

// test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.An_agent_accesses_its_own_and_direct_parents_queues@24
#[tokio::test]
async fn own_and_direct_parent_queues_are_accessible_through_the_tool_port() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    let grandchild = f.runtime.spawn(&child, "grandchild").await;
    let parent_info = json(
        f.call(
            &f.root,
            Create,
            r#"{"name":"parent-work","description":"parent"}"#,
        )
        .await,
    );
    let own_info = json(
        f.call(&child, Create, r#"{"name":"own-work","description":"own"}"#)
            .await,
    );
    assert_eq!(
        json(f.call(&child, Info, r#"{"name":"parent-work"}"#).await),
        parent_info
    );
    assert_eq!(
        json(f.call(&child, Info, r#"{"name":"own-work"}"#).await),
        own_info
    );
    assert_ne!(parent_info["path"], own_info["path"]);
    assert_eq!(
        f.runtime
            .q(&child)
            .list()
            .unwrap()
            .into_iter()
            .map(|q| q.name)
            .collect::<Vec<_>>(),
        ["own-work", "parent-work"]
    );

    f.call(&child, Push, r#"{"name":"own-work","items":["own-item"]}"#)
        .await;
    f.call(
        &child,
        Push,
        r#"{"name":"parent-work","items":["parent-item"]}"#,
    )
    .await;
    assert_eq!(
        json(
            f.call(&child, Take, r#"{"name":"own-work","yield_after_ms":0}"#)
                .await
        )["items"],
        json!(["own-item"])
    );
    assert_eq!(
        json(
            f.call(&child, Take, r#"{"name":"parent-work","yield_after_ms":0}"#)
                .await
        )["items"],
        json!(["parent-item"])
    );
    assert_eq!(
        json(f.call(&f.root, Info, r#"{"name":"parent-work"}"#).await)["size"],
        0
    );
    assert_eq!(
        f.call(&grandchild, Info, r#"{"name":"parent-work"}"#)
            .await
            .text,
        "error: queue: 'parent-work' was not found"
    );
    f.close().await;
}

// test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Qualified_queue_operations_use_target_store_and_bare_names_stay_local@128
#[tokio::test]
async fn explicit_targets_use_exact_local_store_and_bare_names_use_invoking_lookup() {
    let f = Fixture::new().await;
    let local = f.runtime.spawn(&f.root, "local").await;
    let target = f.runtime.spawn(&f.root, "target").await;
    let deep = f.runtime.spawn(&local, "deep").await;
    f.call(&local, Create, r#"{"name":"work","description":"local"}"#)
        .await;
    f.call(&target, Create, r#"{"name":"work","description":"target"}"#)
        .await;
    f.call(&f.root, Create, r#"{"name":"local-only"}"#).await;

    assert_eq!(
        json(
            f.call(&local, Info, r#"{"name":"/root/target/work"}"#)
                .await
        )["description"],
        "target"
    );
    assert_eq!(
        json(f.call(&local, Info, r#"{"name":"work"}"#).await)["description"],
        "local"
    );
    assert_eq!(
        json(
            f.call(
                &local,
                Push,
                r#"{"name":"parent/target/work","items":["target-item"],"close":true}"#
            )
            .await
        )["size"],
        1
    );
    assert_eq!(
        json(
            f.call(
                &deep,
                Take,
                r#"{"name":"parent/parent/target/work","yield_after_ms":0}"#
            )
            .await
        )["items"],
        json!(["target-item"])
    );
    let local_state = f.runtime.q(&local).get("work").unwrap();
    assert_eq!(local_state.size, 0);
    assert!(!local_state.closed);
    assert!(f.runtime.q(&target).get("work").unwrap().closed);

    // target's bare lookup CAN see its parent; explicit Local lookup MUST NOT.
    assert_eq!(
        json(f.call(&target, Info, r#"{"name":"local-only"}"#).await)["size"],
        0
    );
    for (kind, arguments) in [
        (Info, r#"{"name":"target/local-only"}"#),
        (
            Push,
            r#"{"name":"target/local-only","items":["leak"],"close":true}"#,
        ),
        (Take, r#"{"name":"target/local-only","yield_after_ms":0}"#),
    ] {
        assert_eq!(
            f.call(&f.root, kind, arguments).await.text,
            "error: queue: 'local-only' was not found"
        );
    }
    let root_state = f.runtime.q(&f.root).get("local-only").unwrap();
    assert_eq!(root_state.size, 0);
    assert!(!root_state.closed);
    let closed = f
        .log
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|event| event.event == "closed")
        .unwrap()
        .clone();
    assert_eq!(
        closed.agent_session_id,
        target.identity().session.session_id
    );
    f.close().await;
}

// test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Qualified_queue_operations_return_resolution_errors@162
#[tokio::test]
async fn bad_explicit_owner_returns_the_actual_resolver_error_without_fallback() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    for (kind, arguments) in [
        (Info, r#"{"name":"missing/work"}"#),
        (
            Push,
            r#"{"name":"missing/work","items":["leak"],"close":true}"#,
        ),
        (Take, r#"{"name":"missing/work","yield_after_ms":0}"#),
    ] {
        assert_eq!(
            f.call(&f.root, kind, arguments).await.text,
            "error: child agent not found: missing"
        );
    }
    for address in ["/absent/work", "./work", "parent/work", "missing/"] {
        let output = f
            .call(&f.root, Info, &json!({"name":address}).to_string())
            .await;
        assert!(
            output.text.starts_with("error: "),
            "{address}: {}",
            output.text
        );
        assert!(
            !output
                .text
                .contains(f.root.identity().session.session_id.as_str())
        );
    }
    let state = f.runtime.q(&f.root).get("work").unwrap();
    assert_eq!(state.size, 0);
    assert!(!state.closed);
    f.close().await;
}

// test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.A_canceled_push_does_not_mutate_the_queue@306
#[tokio::test]
async fn cancelled_bare_and_explicit_pushes_do_not_add_items_or_close() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    f.call(&f.root, Push, r#"{"name":"work","items":["before"]}"#)
        .await;
    let before = f.runtime.q(&f.root).get("work").unwrap();
    let cancellation = Signal::new();
    cancellation.cancel();
    for name in ["work", "parent/work"] {
        for close in [false, true] {
            let result = f
                .call_cancel(
                    &child,
                    Push,
                    "cancelled",
                    &json!({"name":name,"items":["not-added"],"close":close}).to_string(),
                    &cancellation,
                )
                .await;
            assert_eq!(result, Err(OperationError::Cancelled));
            assert_eq!(f.runtime.q(&f.root).get("work").unwrap(), before);
        }
    }
    assert_eq!(
        json(
            f.call(
                &f.root,
                Take,
                r#"{"name":"work","count":10,"yield_after_ms":0}"#
            )
            .await
        )["items"],
        json!(["before"])
    );
    f.close().await;
}

// test/Parrot.Core.Tests/AgentQueueTests.cs::AgentQueueTests.A_direct_child_can_close_and_drain_its_parents_queue@52
#[tokio::test]
async fn child_closes_and_drains_parent_queue_and_late_items_fail() {
    let f = Fixture::new().await;
    let child = f.runtime.spawn(&f.root, "child").await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    let closed = json(
        f.call(
            &child,
            Push,
            r#"{"name":"work","items":["final"],"close":true}"#,
        )
        .await,
    );
    assert_eq!(closed["closed"], true);
    assert!(f.runtime.q(&f.root).get("work").unwrap().closed);
    assert_eq!(
        f.call(&child, Push, r#"{"name":"work","items":["late"]}"#)
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    let final_item = json(
        f.call(&child, Take, r#"{"name":"work","yield_after_ms":0}"#)
            .await,
    );
    assert_eq!(final_item["items"], json!(["final"]));
    assert_eq!(final_item["closed"], true);
    let drained = json(
        f.call(&child, Take, r#"{"name":"work","yield_after_ms":30000}"#)
            .await,
    );
    assert_eq!(drained["items"], json!([]));
    assert_eq!(drained["closed"], true);
    assert_eq!(drained["size"], 0);
    f.close().await;
}

// test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_take_reports_open_empty_timeout_explicitly@11
#[tokio::test]
async fn open_empty_timeout_is_explicit_and_uses_real_ten_millisecond_polling() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    let tool = f.tool(&f.root, Take);
    let began = Instant::now();
    let output = tool
        .execute(
            invocation("timeout", r#"{"name":"work","yield_after_ms":20}"#),
            context(&NeverCancelled),
        )
        .await
        .unwrap();
    assert!(began.elapsed() >= Duration::from_millis(20));
    let path = f.runtime.q(&f.root).get("work").unwrap().path;
    assert_eq!(
        output.text,
        format!(
            r#"{{"path":{},"name":"work","size":0,"closed":false,"items":[]}}"#,
            serde_json::to_string(path.to_str().unwrap()).unwrap()
        )
    );
    let sleeps = f.clock.sleeps.lock().unwrap().clone();
    assert!(!sleeps.is_empty());
    assert!(
        sleeps
            .iter()
            .all(|duration| *duration <= Duration::from_millis(10))
    );
    let log = f.log.0.lock().unwrap().clone();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].event, "wait_started");
    assert_eq!(log[0].correlation_id.as_deref(), Some("timeout"));
    assert_eq!(log[0].duration_milliseconds, None);
    assert_eq!(log[0].outcome, None);
    assert_eq!(log[1].event, "wait_completed");
    assert_eq!(log[1].correlation_id, log[0].correlation_id);
    assert_eq!(
        log[1].agent_session_id,
        f.root.identity().session.session_id
    );
    assert_eq!(log[1].outcome, Some("timeout"));
    assert_eq!(log[1].error_code, None);
    assert!(log[1].duration_milliseconds.unwrap() >= 20);
    f.close().await;
}

// test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_push_close_allows_prompt_drain_completion@27
#[tokio::test]
async fn close_completes_an_already_waiting_thirty_second_take_within_one_second() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    let take = f.tool(&f.root, Take);
    let waiting = take.invoke(
        invocation("waiting", r#"{"name":"work","yield_after_ms":30000}"#),
        context(&NeverCancelled),
    );
    tokio::pin!(waiting);
    let close = async {
        f.clock.sleeping().await;
        let began = Instant::now();
        let closed = json(
            f.call(&f.root, Push, r#"{"name":"work","items":[],"close":true}"#)
                .await,
        );
        assert_eq!(closed["closed"], true);
        began
    };
    let (output, closed_at) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(&mut waiting, close)
    })
    .await
    .expect("close must not wait for the 30-second polling deadline");
    assert!(closed_at.elapsed() < Duration::from_secs(1));
    let drained = json(output.unwrap());
    assert_eq!(drained["closed"], true);
    assert_eq!(drained["items"], json!([]));
    assert_eq!(
        f.log.0.lock().unwrap().last().unwrap().outcome,
        Some("closed")
    );
    f.close().await;
}

// test/Parrot.Tools.Tests/QueueToolTests.cs::QueueToolTests.Queue_push_adds_final_items_closes_idempotently_and_rejects_late_items@53
#[tokio::test]
async fn final_items_close_is_idempotent_and_ordered_drain_rejects_late_pushes() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    f.call(&f.root, Push, r#"{"name":"work","items":["first"]}"#)
        .await;
    let closed = json(
        f.call(
            &f.root,
            Push,
            r#"{"name":"work","items":["final-a","final-b"],"close":true}"#,
        )
        .await,
    );
    assert_eq!(closed["size"], 3);
    assert_eq!(closed["closed"], true);
    assert_eq!(
        json(
            f.call(&f.root, Push, r#"{"name":"work","items":[],"close":true}"#)
                .await
        ),
        closed
    );
    assert_eq!(
        f.call(&f.root, Push, r#"{"name":"work","items":["late"]}"#)
            .await
            .text,
        "error: queue: 'work' is closed"
    );
    let all = json(
        f.call(
            &f.root,
            Take,
            r#"{"name":"work","count":10,"yield_after_ms":0}"#,
        )
        .await,
    );
    assert_eq!(all["items"], json!(["first", "final-a", "final-b"]));
    assert_eq!(all["closed"], true);
    assert_eq!(all["size"], 0);
    f.close().await;
}

#[tokio::test]
async fn configured_definitions_are_forwarded_and_wire_defaults_and_directions_are_real() {
    let f = Fixture::new().await;
    for kind in [Create, Info, Push, Take] {
        let tool = f.tool(&f.root, kind);
        assert_eq!(
            tool.definition(),
            Fixture::definitions().describe(kind.name()).unwrap()
        );
        assert_eq!(
            tool.parallel_safety(&invocation("definition", "{}")),
            ParallelSafety::Sequential
        );
        assert!(!tool.enabled_after_interruption());
    }
    let created = f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    let path = f.runtime.q(&f.root).get("work").unwrap().path;
    assert_eq!(
        created.text,
        format!(
            r#"{{"path":{},"name":"work","size":0}}"#,
            serde_json::to_string(path.to_str().unwrap()).unwrap()
        )
    );
    assert_eq!(f.call(&f.root, Info, r#"{"name":"work"}"#).await, created);
    // Default push is back, default take is front/count one. A default 30s wait
    // still returns immediately for an already available item.
    f.call(&f.root, Push, r#"{"name":"work","items":["a","b"]}"#)
        .await;
    assert_eq!(
        json(f.call(&f.root, Take, r#"{"name":"work"}"#).await)["items"],
        json!(["a"])
    );
    f.call(
        &f.root,
        Push,
        r#"{"name":"work","items":["x","y"],"direction":"front"}"#,
    )
    .await;
    f.call(
        &f.root,
        Push,
        r#"{"name":"work","items":["c","d"],"direction":"back"}"#,
    )
    .await;
    assert_eq!(
        json(
            f.call(
                &f.root,
                Take,
                r#"{"name":"work","count":2,"direction":"back","yield_after_ms":0}"#
            )
            .await
        )["items"],
        json!(["d", "c"])
    );
    assert_eq!(
        json(
            f.call(
                &f.root,
                Take,
                r#"{"name":"work","count":10,"direction":"front","yield_after_ms":0}"#
            )
            .await
        )["items"],
        json!(["y", "x", "b"])
    );
    assert!(f.clock.sleeps.lock().unwrap().is_empty());
    f.close().await;
}

#[tokio::test]
async fn distinct_queue_inputs_reject_other_tools_fields_without_mutation() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    for (kind, arguments, field) in [
        (Info, r#"{"name":"work","items":[]}"#, "items"),
        (Create, r#"{"name":"other","enabled":true}"#, "enabled"),
        (Info, r#"{"name":"work","close":true}"#, "close"),
        (Push, r#"{"name":"work","items":[],"count":1}"#, "count"),
        (
            Take,
            r#"{"name":"work","items":[],"yield_after_ms":0}"#,
            "items",
        ),
    ] {
        assert_eq!(
            f.call(&f.root, kind, arguments).await.text,
            format!("error: The JSON property '{field}' could not be mapped to the tool input.")
        );
    }
    for arguments in [
        r#"{"name":"work"}"#,
        r#"{"name":"work","items":[],"source_file":"items.txt"}"#,
    ] {
        assert_eq!(
            f.call(&f.root, Push, arguments).await.text,
            "error: Tool arguments require exactly one of 'items' or 'source_file'."
        );
    }
    assert_eq!(f.runtime.q(&f.root).get("work").unwrap().size, 0);
    assert!(f.runtime.q(&f.root).get("other").is_err());
    f.close().await;
}

#[tokio::test]
async fn take_diagnostics_preserve_order_source_classification_and_cancellation() {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    // Explicit resolver errors precede count/direction/yield validation.
    assert_eq!(
        f.call(
            &f.root,
            Take,
            r#"{"name":"missing/work","count":0,"direction":"wrong","yield_after_ms":-1}"#
        )
        .await
        .text,
        "error: child agent not found: missing"
    );
    let cancellation = Signal::new();
    cancellation.cancel();
    assert_eq!(
        f.call_cancel(
            &f.root,
            Take,
            "cancelled",
            r#"{"name":"work","yield_after_ms":0}"#,
            &cancellation
        )
        .await,
        Err(OperationError::Cancelled)
    );
    f.call(&f.root, Push, r#"{"name":"work","items":["item"]}"#)
        .await;
    f.call(&f.root, Take, r#"{"name":"work","yield_after_ms":0}"#)
        .await;
    let log = f.log.0.lock().unwrap().clone();
    assert_eq!(
        log.iter().map(|event| event.event).collect::<Vec<_>>(),
        [
            "wait_started",
            "wait_completed",
            "wait_started",
            "wait_completed",
            "wait_started",
            "wait_completed"
        ]
    );
    assert_eq!(log[1].outcome, Some("failed"));
    assert_eq!(log[1].error_code, Some("unexpected"));
    assert_eq!(log[3].outcome, Some("cancelled"));
    assert_eq!(log[3].error_code, Some("cancelled"));
    assert_eq!(log[3].correlation_id.as_deref(), Some("cancelled"));
    assert_eq!(log[5].outcome, Some("delivered"));
    assert_eq!(log[5].error_code, None);
    for pair in log.chunks_exact(2) {
        assert_eq!(pair[0].agent_session_id, pair[1].agent_session_id);
        assert_eq!(pair[0].correlation_id, pair[1].correlation_id);
        assert_eq!(pair[0].duration_milliseconds, None);
        assert!(pair[1].duration_milliseconds.is_some());
    }
    f.close().await;
}
