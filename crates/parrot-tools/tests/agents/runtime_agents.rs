//! Source pins: SubagentTests.cs (spawn/reuse/fork/path/send cases),
//! AgentInterruptToolTests.cs, AgentStatusToolTests.cs and their tool wrappers in
//! p01-infrastructure-reference/.tmp/reference-build/source. These exercise a
//! TEST-local bridge; they do not close the P08 production-binding or P05 security gates.
use std::sync::Arc;

use parrot_domain::contracts::tools::Tool;
use parrot_runtime::agent_turn::{lifecycle::ExecutionStatus, services::SelectionSource};
use parrot_storage::{
    conversation::{ConversationMessage, MessageRole},
    events::NeverCancelled,
};
use parrot_tools::agents::AgentToolKind;
use serde_json::{Value, json};

use super::{CANCELLED, runtime_support::*};

#[tokio::test]
async fn actual_model_selector_adapter_returns_requested_alias_not_canonical_model() {
    let harness = Harness::default();
    assert_eq!(
        parrot_tools::agents::ModelSelector::resolve(harness.models.0.as_ref(), "fast").unwrap(),
        "fast"
    );
    assert_eq!(
        parrot_tools::agents::ModelSelector::resolve(harness.models.0.as_ref(), "slow").unwrap(),
        "slow"
    );
    assert!(matches!(
        parrot_tools::agents::ModelSelector::resolve(harness.models.0.as_ref(), "broken"),
        Err(parrot_tools::agents::AgentToolError::Provider(_))
    ));
    harness.close().await;
}

#[tokio::test]
async fn actual_spawn_preserves_captured_alias_fork_batch_running_and_normalized_idle_reuse() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        append(&root, ConversationMessage::text(MessageRole::User, "before captured batch")).await;
        let sequence = spawn_batch(&root).await;
        append(&root, ConversationMessage::text(MessageRole::User, "after captured batch")).await;
        let captured = context(&root, &NeverCancelled);
        let mut changed = root.selection().current().unwrap();
        changed.model = h.models.0.resolve("slow").unwrap();
        root.session().update_selection(changed).unwrap();
        let gate = Gate::new();
        h.provider.plans.lock().unwrap().push_back(StreamPlan { gate: Some(gate.clone()), ..Default::default() });
        let tool = h.tool(AgentToolKind::Spawn, &root, false);
        let mut request = invocation(json!({"prompt":"first prompt", "agent":"explore", "name":"  Research NOTE!  ", "scope":"bounded research", "fork":"full"}));
        request.assistant_sequence = sequence;
        let text = tool.invoke(request, captured).await.unwrap();
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["name"], "research-note");
        assert_eq!(result["depth"], 1);
        assert_eq!(result["status"], "running");
        let child = root.children().resolve("research-note").unwrap();
        gate.entered().await;
        assert!(child.session().is_active());
        assert!(!text.contains(&child.identity().session.session_id));
        assert_eq!(result["output"], services(&child).output_path.display().to_string());
        let selected = child.selection().capture().unwrap();
        assert_eq!(selected.profile.id, "worker");
        assert_eq!(selected.model.requested_selector.value, "fast");
        assert_eq!(selected.model.canonical_base(), "scripted/first");
        assert_eq!(h.provider.requests.lock().unwrap()[0].model, "first");
        assert!(child.identity().prompt_scope.format(1).unwrap().contains("bounded research"));
        let seeded = history_text(&child).await;
        assert!(seeded.contains(&"before captured batch".into()));
        assert!(seeded.contains(&"first prompt".into()));
        assert!(!seeded.iter().any(|text| text.contains("after captured") || text.contains("current spawn batch")));
        let busy = tool.invoke(invocation(json!({"prompt":"busy", "agent":"worker", "name":"RESEARCH note"})), context(&root, &NeverCancelled)).await.unwrap();
        assert!(busy.contains("busy"), "{busy}");
        assert_eq!(root.children().snapshot_children().len(), 1);
        let mut wrong_call = invocation(json!({"prompt":"work", "agent":"worker", "name":"wrong-boundary", "fork":"full"}));
        wrong_call.assistant_sequence = sequence;
        wrong_call.call_id = "unavailable-call".into();
        let rejected = tool.invoke(wrong_call, context(&root, &NeverCancelled)).await.unwrap();
        assert!(rejected.contains("captured fork tool batch is unavailable"), "{rejected}");
        assert!(root.children().find("wrong-boundary").is_none());
        gate.open();
        assert_eq!(services(&child).settle().await.status, ExecutionStatus::Succeeded);
        let original = services(&child);
        let reused = tool.invoke(invocation(json!({"prompt":"follow up", "agent":"revised", "name":"Research Note", "model":"slow", "fork":"absent-checkpoint", "scope":"must not replace scope"})), context(&root, &NeverCancelled)).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&reused).unwrap()["name"], "research-note");
        let resumed = root.children().resolve("research-note").unwrap();
        assert!(Arc::ptr_eq(&child, &resumed));
        assert!(Arc::ptr_eq(&original, &services(&resumed)));
        let selected = resumed.selection().capture().unwrap();
        assert_eq!(selected.profile.id, "revised");
        assert_eq!(selected.model.requested_selector.value, "slow");
        assert_eq!(selected.model.canonical_base(), "scripted/second");
        assert!(!resumed.identity().prompt_scope.format(1).unwrap().contains("must not replace"));
        assert_eq!(services(&resumed).settle().await.status, ExecutionStatus::Succeeded);
        assert!(history_text(&resumed).await.contains(&"first prompt".into()));
        assert!(history_text(&resumed).await.contains(&"follow up".into()));
        h.close().await;
    }).await;
}

#[tokio::test]
async fn actual_spawn_rejects_ineligible_profiles_unknown_models_and_fifth_depth() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let tool = h.tool(AgentToolKind::Spawn, &root, false);
        for profile in ["foreground", "not-agent-selectable", "missing"] {
            let text = tool.invoke(invocation(json!({"prompt":"work", "agent":profile, "name":"rejected"})), context(&root, &NeverCancelled)).await.unwrap();
            assert!(text.starts_with("error:"), "{text}");
            assert!(text.contains("not agent selectable"), "{text}");
        }
        let text = tool.invoke(invocation(json!({"prompt":"work", "agent":"worker", "name":"rejected", "model":"missing-alias"})), context(&root, &NeverCancelled)).await.unwrap();
        assert!(text.starts_with("error:"), "{text}");
        assert!(root.children().snapshot_children().is_empty());
        let mut parent = root.clone();
        for depth in 1..=4 {
            parent = h.child(&parent, &format!("level-{depth}")).await;
            assert_eq!(parent.identity().session.depth, depth);
        }
        let text = h.tool(AgentToolKind::Spawn, &parent, false).invoke(invocation(json!({"prompt":"too deep", "agent":"worker", "name":"level-5"})), context(&parent, &NeverCancelled)).await.unwrap();
        assert!(text.contains("depth limit"), "{text}");
        assert!(parent.children().snapshot_children().is_empty());
        assert!(h.provider.requests.lock().unwrap().is_empty());
        h.close().await;
    }).await;
}

#[tokio::test]
async fn actual_send_resolves_scope_paths_parent_aliases_and_enforces_every_ancestor_without_ids() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let branch = h.child(&root, "branch").await;
        let leaf = h.child(&branch, "leaf").await;
        let sibling = h.child(&root, "sibling").await;
        let collision = h.child(&leaf, "parent").await;
        let denied = h.tool(AgentToolKind::Send, &leaf, false);
        for name in ["parent", "branch", "..", "../..", "/root", "/root/branch"] {
            let text = denied
                .invoke(
                    invocation(json!({"name":name, "message":"blocked"})),
                    context(&leaf, &NeverCancelled),
                )
                .await
                .unwrap();
            assert!(
                text.contains("sending to the parent agent is disabled"),
                "{name}: {text}"
            );
            for scope in [&root, &branch, &leaf, &sibling] {
                assert!(!text.contains(&scope.identity().session.session_id));
            }
        }
        assert!(history_text(&collision).await.is_empty());
        assert!(h.provider.requests.lock().unwrap().is_empty());
        let permitted = h.tool(AgentToolKind::Send, &leaf, true);
        for name in ["parent", "branch", "../..", "/root/branch"] {
            let text = permitted
                .invoke(
                    invocation(json!({"name":name, "message":format!("allowed {name}")})),
                    context(&leaf, &NeverCancelled),
                )
                .await
                .unwrap();
            let target = if name == "../.." { &root } else { &branch };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["name"],
                target.identity().session.name
            );
            assert_eq!(
                services(target).settle().await.status,
                ExecutionStatus::Succeeded
            );
            assert!(
                history_text(target)
                    .await
                    .contains(&format!("allowed {name}"))
            );
        }
        for name in ["../../sibling", "/root/sibling"] {
            let text = denied
                .invoke(
                    invocation(json!({"name":name, "message":format!("scoped {name}")})),
                    context(&leaf, &NeverCancelled),
                )
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["name"],
                "sibling"
            );
            services(&sibling).settle().await;
        }
        for name in [
            &root.identity().session.session_id,
            &sibling.identity().session.session_id,
        ] {
            let text = denied
                .invoke(
                    invocation(json!({"name":name, "message":"no canonical lookup"})),
                    context(&leaf, &NeverCancelled),
                )
                .await
                .unwrap();
            assert!(text.starts_with("error:"), "{text}");
        }
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_send_reports_followup_vs_steering_and_accepts_exact_utf8_boundary_only() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let child = h.child(&root, "child").await;
        let gate = Gate::new();
        h.provider.plans.lock().unwrap().push_back(StreamPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        });
        let tool = h.tool(AgentToolKind::Send, &root, false);
        let boundary = "🦜".repeat(8192);
        assert_eq!(boundary.len(), 32768);
        let fresh = tool
            .invoke(
                invocation(json!({"name":"child", "message":boundary})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        let fresh: Value = serde_json::from_str(&fresh).unwrap();
        assert_eq!(fresh["name"], "child");
        assert!(
            fresh["message_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(fresh.get("status").is_none());
        gate.entered().await;
        let steering = tool
            .invoke(
                invocation(json!({"name":"child", "message":"steer while running"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        let steering: Value = serde_json::from_str(&steering).unwrap();
        assert_eq!(steering["name"], "child");
        assert!(
            steering["message_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(steering.get("status").is_none());
        let before = child.history().conversation().await.unwrap().len();
        let rejected = tool
            .invoke(
                invocation(json!({"name":"child", "message":format!("{boundary}x")})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(rejected.contains("32768 UTF-8 bytes"));
        assert_eq!(child.history().conversation().await.unwrap().len(), before);
        gate.open();
        assert_eq!(
            services(&child).settle().await.status,
            ExecutionStatus::Succeeded
        );
        let followup = tool
            .invoke(
                invocation(json!({"name":"child", "message":"idle follow up"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert_eq!(followup, r#"{"name":"child","status":"running"}"#);
        assert_eq!(
            services(&child).settle().await.status,
            ExecutionStatus::Succeeded
        );
        let texts = history_text(&child).await;
        assert_eq!(texts.iter().filter(|text| **text == boundary).count(), 1);
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.as_str() == "steer while running")
                .count(),
            1
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_send_compares_live_sibling_selections_without_claiming_filesystem_authority() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let sender = h.child(&root, "sender").await;
        let target = h.child(&root, "target").await;
        let mut narrow = sender.selection().current().unwrap();
        narrow.security = Security::new(1);
        sender.session().update_selection(narrow).unwrap();
        let tool = h.tool(AgentToolKind::Send, &sender, true);
        for name in ["../target", "/root/target"] {
            let text = tool
                .invoke(
                    invocation(json!({"name":name, "message":"must not arrive"})),
                    context(&sender, &NeverCancelled),
                )
                .await
                .unwrap();
            assert!(text.contains("more permissive security profile"), "{text}");
            assert!(!text.contains(&target.identity().session.session_id));
        }
        assert!(history_text(&target).await.is_empty());
        assert!(h.provider.requests.lock().unwrap().is_empty());
        let mut restricted = target.selection().current().unwrap();
        restricted.security = Security::new(1);
        target.session().update_selection(restricted).unwrap();
        let text = tool
            .invoke(
                invocation(json!({"name":"../target", "message":"now permitted"})),
                context(&sender, &NeverCancelled),
            )
            .await
            .unwrap();
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["name"], "target");
        assert!(
            result["message_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        services(&target).settle().await;
        assert!(
            history_text(&target)
                .await
                .contains(&"now permitted".into())
        );
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_interrupt_is_direct_only_returns_before_unwind_and_ignores_caller_cancellation() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let child = h.child(&root, "child").await;
        let grandchild = h.child(&child, "grandchild").await;
        let gate = Gate::new();
        let unwind = Gate::new();
        h.provider.plans.lock().unwrap().push_back(StreamPlan {
            gate: Some(gate.clone()),
            unwind: Some(unwind.clone()),
            ..Default::default()
        });
        h.tool(AgentToolKind::Send, &root, false)
            .invoke(
                invocation(json!({"name":"child", "message":"running"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        gate.entered().await;
        let interrupt = h.tool(AgentToolKind::Interrupt, &root, false);
        for name in [
            "parent",
            "child/grandchild",
            "/root/child",
            &child.identity().session.session_id,
            &grandchild.identity().session.session_id,
        ] {
            let text = interrupt
                .invoke(invocation(json!({"name":name})), context(&root, &CANCELLED))
                .await
                .unwrap();
            assert!(text.starts_with("error:"), "{name}: {text}");
            assert!(child.session().is_active());
        }
        let text = interrupt
            .invoke(
                invocation(json!({"name":"  child  "})),
                context(&root, &CANCELLED),
            )
            .await
            .unwrap();
        assert!(
            text.contains("Interrupt requested for child agent 'child'"),
            "{text}"
        );
        unwind.entered().await;
        let session = services(&child);
        assert!(
            !session
                .executions
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .is_finished()
        );
        assert_eq!(
            session.adapter.session().state(),
            parrot_runtime::agent_turn::lifecycle::DrainState::Interrupting
        );
        unwind.open();
        assert_eq!(session.settle().await.status, ExecutionStatus::Canceled);
        assert!(!child.session().is_active());
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_status_reads_running_scope_and_only_its_active_direct_children() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let child = h.child(&root, "child").await;
        let active = h.child(&child, "active").await;
        let grandchild = h.child(&active, "grandchild").await;
        let idle = h.child(&child, "idle").await;
        let gate = Gate::new();
        h.provider.plans.lock().unwrap().push_back(StreamPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        });
        h.tool(AgentToolKind::Send, &child, false)
            .invoke(
                invocation(json!({"name":"active", "message":"work"})),
                context(&child, &NeverCancelled),
            )
            .await
            .unwrap();
        gate.entered().await;
        let deep_gate = Gate::new();
        h.provider.plans.lock().unwrap().push_back(StreamPlan {
            gate: Some(deep_gate.clone()),
            ..Default::default()
        });
        h.tool(AgentToolKind::Send, &active, false)
            .invoke(
                invocation(json!({"name":"grandchild", "message":"deep work"})),
                context(&active, &NeverCancelled),
            )
            .await
            .unwrap();
        deep_gate.entered().await;
        let status = h.tool(AgentToolKind::Status, &root, false);
        let summary = status
            .invoke(
                invocation(json!({"name":"child"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(
            summary.contains("Active direct subagents:\n- active"),
            "{summary}"
        );
        assert!(!summary.contains("grandchild") && !summary.contains("- idle"));
        let running = status
            .invoke(
                invocation(json!({"name":"child/active"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(running.contains("Lifecycle: running"), "{running}");
        assert!(running.contains("Current provider request duration:"));
        assert!(running.contains("Active direct subagents:\n- grandchild"));
        assert!(running.contains("Active processes: none"));
        for scope in [&child, &active, &grandchild, &idle] {
            assert!(!running.contains(&scope.identity().session.session_id));
        }
        gate.open();
        deep_gate.open();
        services(&active).settle().await;
        services(&grandchild).settle().await;
        h.close().await;
    })
    .await;
}

#[tokio::test]
async fn actual_status_scope_aliases_share_per_session_cursor_and_bound_recent_not_full_output() {
    bounded(async {
        let h = Harness::default();
        let root = h.root("root").await;
        let child = h.child(&root, "child").await;
        let sibling = h.child(&root, "sibling").await;
        let send = h.tool(AgentToolKind::Send, &root, false);
        for index in 1..=6 {
            h.provider.plans.lock().unwrap().push_back(StreamPlan {
                answer: format!("entry-{index}"),
                ..Default::default()
            });
            send.invoke(
                invocation(json!({"name":"child", "message":format!("request-{index}")})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
            services(&child).settle().await;
        }
        let status = h.tool(AgentToolKind::Status, &root, false);
        let first = status
            .invoke(
                invocation(json!({"name":"child"})),
                context(&root, &CANCELLED),
            )
            .await
            .unwrap();
        assert!(first.contains("Name: child"));
        assert!(first.contains("Lifecycle: idle"));
        assert!(first.contains("Last outcome: succeeded"));
        assert!(!first.contains("entry-1"));
        for index in 2..=6 {
            assert!(first.contains(&format!("entry-{index}")));
        }
        let again = status
            .invoke(
                invocation(json!({"name":"/root/child"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(
            again.contains("4 earlier entries already reported"),
            "{again}"
        );
        assert!(!again.contains("entry-5"));
        assert!(again.contains("entry-6"));
        for index in 1..=2 {
            h.provider.plans.lock().unwrap().push_back(StreamPlan {
                answer: format!("sibling-{index}"),
                ..Default::default()
            });
            send.invoke(
                invocation(json!({"name":"sibling", "message":"work"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
            services(&sibling).settle().await;
        }
        let independent = status
            .invoke(
                invocation(json!({"name":"sibling"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(independent.contains("sibling-1") && independent.contains("sibling-2"));
        assert!(!independent.contains("earlier entries already reported"));
        let huge = format!("BEGIN-UNTRUNCATED-{}-END-UNTRUNCATED", "🦜".repeat(6000));
        h.provider.plans.lock().unwrap().push_back(StreamPlan {
            answer: huge.clone(),
            ..Default::default()
        });
        send.invoke(
            invocation(json!({"name":"child", "message":"large answer"})),
            context(&root, &NeverCancelled),
        )
        .await
        .unwrap();
        services(&child).settle().await;
        let bounded = status
            .invoke(
                invocation(json!({"name":"/root/child"})),
                context(&root, &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(
            bounded.contains("older entries omitted to stay within 1K tokens"),
            "{bounded}"
        );
        assert!(!bounded.contains("BEGIN-UNTRUNCATED"));
        let full = std::fs::read_to_string(&services(&child).output_path).unwrap();
        assert!(full.contains("entry-1"));
        assert!(full.contains(&huge));
        for name in [
            &child.identity().session.session_id,
            &sibling.identity().session.session_id,
        ] {
            let denied = status
                .invoke(
                    invocation(json!({"name":name})),
                    context(&root, &NeverCancelled),
                )
                .await
                .unwrap();
            assert!(denied.starts_with("error:"));
        }
        h.close().await;
    })
    .await;
}
