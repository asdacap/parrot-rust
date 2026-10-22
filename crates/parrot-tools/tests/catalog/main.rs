mod binding;
mod support;

use binding::Fixture;
use parrot_domain::contracts::{async_ops::OperationError, tools::ParallelSafety};
use parrot_provider::{chat_completions, sessions::model::Request};
use parrot_tools::{
    catalog::{self, BUILTIN_NAMES, Invocation},
    files::{FileToolOutcome, ImageBudget},
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn native_configuration_and_retained_factories_preserve_evolved_source_order_and_definitions()
{
    let fixture = Fixture::new("", true, 0);
    let definitions = catalog::configured_definitions(&fixture.configuration);
    let original_order: Value =
        serde_json::from_str(include_str!("fixtures/source-factory-order.json")).unwrap();
    assert_eq!(
        original_order["factories"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| match row["name"].as_str().unwrap() {
                "answer" => vec!["ask", "reply"],
                name => vec![name],
            })
            .collect::<Vec<_>>(),
        BUILTIN_NAMES
    );
    assert_eq!(
        include_bytes!("fixtures/predefined_config.runtime.source.yaml"),
        include_bytes!("../../../../assets/config/predefined_config.runtime.yaml")
    );
    assert_eq!(definitions.definitions().len(), 31);
    assert_eq!(
        fixture
            .catalog
            .factories()
            .iter()
            .map(|f| f.name())
            .collect::<Vec<_>>(),
        BUILTIN_NAMES
    );
    let snapshot = fixture.catalog.snapshot().unwrap();
    assert_eq!(
        snapshot
            .definitions()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        BUILTIN_NAMES
    );
    for factory in fixture.catalog.factories() {
        let configured = &fixture.configuration.runtime.tools[factory.name()];
        assert_eq!(factory.definition().description, configured.description);
        assert_eq!(
            factory.definition().parameters_json,
            configured.parameters_json
        );
        assert_eq!(factory.create().definition(), factory.definition());
        assert!(Arc::ptr_eq(
            &factory.create(),
            &snapshot.find(factory.name()).unwrap().create()
        ));
        assert!(Arc::ptr_eq(&factory.create(), &factory.create()));
        assert!(factory.supports());
    }
    assert_eq!(
        definitions.describe("missing").unwrap_err().to_string(),
        "tools.missing is not defined"
    );
    assert!(Arc::ptr_eq(
        &snapshot.find("bash").unwrap().create(),
        &snapshot.find("exec_command").unwrap().create()
    ));
    assert!(snapshot.find("Bash").is_none());
    assert!(snapshot.find("answer").is_none());
    assert!(!fixture.configuration.runtime.tools.contains_key("answer"));
    assert!(!snapshot.definitions().iter().any(|d| d.name == "bash"));
    assert!(fixture.services.calls().is_empty());
    fixture.close().await;
}

const READ_OVERRIDE: &str = "tools:\n  read:\n    description: Read configured content.\n    parameters:\n      properties:\n        path:\n          type: integer\n          minimum: 7\n          description: Configured path prose.\n";
const GLOB_DESCRIPTION: &str = "Find paths beneath an optional root with deterministic glob matching, including **. Relative roots resolve within the workspace.";

#[tokio::test]
async fn exact_t26_native_configuration_actual_read_glob_catalog_to_real_provider_prepare() {
    let fixture = Fixture::new(READ_OVERRIDE, true, 0);
    let snapshot = fixture.catalog.snapshot().unwrap();
    let selected = snapshot.permitted_by(|name| matches!(name, "read" | "glob"));
    let request = Request {
        model: "vendor/model".into(),
        tools: selected.definitions(),
        ..Request::default()
    };
    let prepared = chat_completions::prepare(&request, "test").unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    let tools = body["tools"].as_array().unwrap();
    let single = |name: &str| {
        let rows = tools
            .iter()
            .filter(|tool| tool["function"]["name"] == name)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        rows[0]["function"].clone()
    };
    let read = single("read");
    let glob = single("glob");
    assert_eq!(read["description"], "Read configured content.");
    assert_eq!(
        read["parameters"]["properties"]["path"]["description"],
        "Configured path prose."
    );
    assert_eq!(read["parameters"]["properties"]["path"]["type"], "integer");
    assert_eq!(read["parameters"]["properties"]["path"]["minimum"], 7);
    assert_eq!(glob["description"], GLOB_DESCRIPTION);
    // Actual native wrappers remain the retained entries, not neutral source JSON.
    assert_eq!(
        fixture
            .invoke("read", json!({"path":"file.txt"}))
            .await
            .unwrap()
            .text,
        "1: alpha\n2: beta\ntotal lines in file: 2\n"
    );
    assert_eq!(
        fixture
            .invoke("glob", json!({"pattern":"**"}))
            .await
            .unwrap()
            .text,
        "file.txt\n"
    );
    fixture.close().await;
}

#[tokio::test]
async fn source_support_send_definition_and_interruption_flags_are_bound_without_recreating_tools()
{
    let fixture = Fixture::new("", false, 1);
    assert_eq!(fixture.catalog.factories().len(), 31);
    let snapshot = fixture.catalog.snapshot().unwrap();
    assert_eq!(snapshot.factories().len(), 29);
    assert!(snapshot.find("request_write_permission").is_none());
    assert!(snapshot.find("question").is_none());
    assert!(snapshot.find("answer").is_none());
    assert!(snapshot.find("ask").is_some());
    assert!(snapshot.find("reply").is_some());
    let source = &fixture.configuration.runtime.tools["agent_send"];
    let send = snapshot.find("agent_send").unwrap();
    assert_eq!(send.definition().parameters_json, source.parameters_json);
    assert_eq!(
        send.definition().description,
        fixture
            .templates
            .render(
                "agent-send-tool.description-without-parent",
                &[],
                &support::NoCancel
            )
            .unwrap()
    );
    assert_ne!(send.definition().description, source.description);
    let surviving = snapshot.enabled_after_interruption();
    assert_eq!(
        surviving
            .factories()
            .iter()
            .map(|f| f.name())
            .collect::<Vec<_>>(),
        vec![
            "interrupt_process",
            "ask",
            "reply",
            "agent_send",
            "agent_interrupt",
            "agent_status",
            "wait"
        ]
    );
    assert_eq!(snapshot.factories().len(), 29);
    for factory in surviving.factories() {
        assert!(Arc::ptr_eq(
            &factory.create(),
            &snapshot.find(factory.name()).unwrap().create()
        ));
    }
    let exec = snapshot.find("exec_command").unwrap().create();
    assert_eq!(
        exec.parallel_safety(&Invocation::new("call", r#"{"command":"printf hello"}"#)),
        ParallelSafety::ParallelSafe
    );
    assert_eq!(
        exec.parallel_safety(&Invocation::new("call", r#"{"command":"rm file"}"#)),
        ParallelSafety::Sequential
    );
    for name in ["read", "read_image", "glob", "web_fetch"] {
        assert_eq!(
            snapshot
                .find(name)
                .unwrap()
                .create()
                .parallel_safety(&Invocation::new("call", "{}")),
            ParallelSafety::ParallelSafe
        );
    }
    for name in [
        "parent",
        "parent/sibling",
        "/root-name/child",
        "child/grandchild",
    ] {
        assert_eq!(
            fixture
                .invoke("ask", json!({"name":name,"message":"question"}))
                .await
                .unwrap()
                .text,
            "catalog reply"
        );
        assert!(
            fixture.services.calls().iter().any(|call| {
                call.operation == "ask" && call.detail == format!("{name} question")
            })
        );
    }
    assert_eq!(
        fixture
            .invoke("reply", json!({"request_id":"request","message":"yes"}))
            .await
            .unwrap()
            .text,
        "Replied to ask request."
    );
    fixture.close().await;
}

#[tokio::test]
async fn every_actual_factory_dispatches_its_wrapper_and_preserves_family_results_and_defaults() {
    let fixture = Fixture::new("", true, 0);
    let cases = [
        ("exec_command", json!({"command":"printf catalog"})),
        ("write_stdin", json!({"name":"missing","input":"hello"})),
        ("interrupt_process", json!({"name":"missing"})),
        (
            "monitor",
            json!({"command":"printf catalog","name":"watch","description":"Catalog monitor"}),
        ),
        ("question", json!({"questions":[{"prompt":"Which?"}]})),
        ("ask", json!({"name":"parent/sibling","message":"Which?"})),
        (
            "reply",
            json!({"request_id":"catalog-request","message":"yes"}),
        ),
        ("read", json!({"path":"file.txt"})),
        ("read_image", json!({"path":"pixel.png"})),
        (
            "imagegen",
            json!({"prompt":"A bird","output_path":"generated.png"}),
        ),
        ("glob", json!({"pattern":"**"})),
        ("write", json!({"path":"old.txt","content":"created"})),
        (
            "edit",
            json!({"path":"old.txt","old_string":"created","new_string":"new"}),
        ),
        ("web_fetch", json!({"url":"https://catalog.invalid/plain"})),
        (
            "agent_spawn",
            json!({"agent":"worker","name":"child","prompt":"hello"}),
        ),
        (
            "set_agent_tasks",
            json!({"tasks":[{"name":"work","description":"Do work","payload":"instructions","acceptance_criteria":"done","state":"pending"}]}),
        ),
        ("get_agent_tasks", json!({})),
        ("set_checkpoint", json!({"title":"before"})),
        (
            "set_exit_reminder",
            json!({"title":"finish","description":"Finish work"}),
        ),
        ("clear_exit_reminder", json!({"title":"finish"})),
        ("agent_send", json!({"name":"child","message":"hello"})),
        ("agent_interrupt", json!({"name":"child"})),
        ("agent_status", json!({"name":"child"})),
        ("wait", json!({})),
        ("status", json!({})),
        ("compact_context", json!({})),
        ("queue_create", json!({"name":"work"})),
        ("queue_info", json!({"name":"work"})),
        ("queue_push", json!({"name":"work","items":["item"]})),
        ("queue_take", json!({"name":"work"})),
        (
            "request_write_permission",
            json!({"paths":["target.txt"],"reason":"Write output"}),
        ),
    ];
    assert_eq!(cases.iter().map(|c| c.0).collect::<Vec<_>>(), BUILTIN_NAMES);
    let mut observed = serde_json::Map::new();
    for (name, args) in cases {
        let result = fixture
            .invoke(name, args)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!result.text.is_empty(), "{name}");
        match name {
            "exec_command" | "write_stdin" | "interrupt_process" | "monitor" => {
                assert!(result.text.starts_with("error:"), "{name}: {}", result.text)
            }
            "read" => assert_eq!(result.text, "1: alpha\n2: beta\ntotal lines in file: 2\n"),
            "read_image" => {
                assert_eq!(result.image_artifacts.len(), 1);
                assert_eq!(result.image_artifacts[0].origin, "read_image");
                assert_eq!(result.image_artifacts[0].display_name, "pixel.png");
                assert_eq!(result.outcome, FileToolOutcome::Finished);
            }
            "imagegen" => assert!(result.text.contains("generated.png")),
            "glob" => assert_eq!(result.text, "file.txt\n"),
            "web_fetch" => assert!(result.text.contains("catalog web")),
            "set_checkpoint" => assert_eq!(result.text, "before"),
            "set_agent_tasks" => assert_eq!(
                result.text,
                "AgentTask update accepted (1 tasks). State changes are delivered as notifications."
            ),
            "wait" => assert_eq!(result.text, "wait interrupted"),
            "status" => assert_eq!(result.text, "catalog runtime\n\ncatalog statistics"),
            "queue_take" => {
                let value: Value = serde_json::from_str(&result.text).unwrap();
                assert_eq!(value["items"], json!(["item"]));
            }
            _ => {}
        }
        if name != "read_image" {
            assert!(result.image_artifacts.is_empty());
        }
        assert!(result.yielded_process.is_none());
        observed.insert(name.into(), Value::String(result.text));
    }
    let observed = Value::Object(observed);
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/native-result-snapshot.json")).unwrap();
    assert_eq!(
        observed, expected,
        "native aggregate regression snapshot; not an independent source observation"
    );
    println!("CATALOG_RESULT_OBSERVATION={observed}");
    let calls = fixture.services.calls();
    assert!(!calls.iter().any(|call| call.operation == "model"));
    assert!(
        calls
            .iter()
            .any(|call| call.operation == "spawn"
                && call.detail.contains("model: \"captured-alias\""))
    );
    assert!(
        calls
            .iter()
            .any(|call| call.operation == "wait"
                && call.detail == "WaitInput { duration_ms: 300000 }")
    );
    for (request, captured) in &fixture.services.state.lock().unwrap().launches {
        assert_eq!(*captured, 71);
        assert!(request.environment.is_empty());
        assert_eq!(
            request.terminal_mode,
            parrot_tools::process::execution::TerminalMode::Pipe
        );
    }
    for call in &calls {
        if let Some(id) = call.security_id {
            assert_eq!(id, 71, "{call:?}");
        }
        if let Some(id) = call.selection_id {
            assert_eq!(id, 89, "{call:?}");
        }
    }
    for operation in [
        "read",
        "mutation_read",
        "mutation_write",
        "spawn",
        "question",
        "ask",
        "reply",
        "wait",
        "checkpoint",
        "runtime_status",
        "statistics_status",
        "compact",
        "image_generate",
        "task_update",
        "queue_create",
        "queue_info",
        "queue_push",
        "queue_take",
        "permission_request",
        "process_launch",
    ] {
        assert!(
            calls.iter().any(|call| call.operation == operation),
            "missing {operation}: {calls:?}"
        );
    }
    assert_eq!(
        fixture.services.state.lock().unwrap().task_boundary,
        Some((9_007_199_254_740_993, "catalog-call".into()))
    );
    assert_eq!(
        fixture.services.state.lock().unwrap().checkpoints,
        vec![(
            "before".into(),
            9_007_199_254_740_993,
            "catalog-call".into()
        )]
    );
    assert_eq!(fixture.web.sessions.load(Ordering::SeqCst), 1);
    assert_eq!(*fixture.web.requests.lock().unwrap(), vec!["GET"]);
    fixture.close().await;
}

#[tokio::test]
async fn retained_identity_budget_templates_and_cancellation_survive_binding() {
    let fixture = Fixture::new("", true, 0);
    let snapshot = fixture.catalog.snapshot().unwrap();
    let read = snapshot.find("read_image").unwrap().create();
    let budget = Arc::new(parrot_domain::values::tools::ToolCycleImageBudget::new(1));
    assert!(!budget.try_accept(2).unwrap());
    let mut invocation = Invocation::new("image-budget", "not json");
    invocation.image_budget = Some(Arc::new(ImageBudget::new(
        budget,
        fixture.templates.clone(),
    )));
    let output = read
        .invoke(
            invocation,
            parrot_domain::contracts::security::InvocationContext::new(
                support::Snapshot { id: 1 },
                catalog::Selection {
                    requested_model: "alias".into(),
                    scoped: support::Selection { id: 2 },
                },
                &support::NoCancel,
            ),
        )
        .await
        .unwrap();
    assert_eq!(output.outcome, FileToolOutcome::ImageBudgetExceeded);
    assert!(output.image_artifacts.is_empty());
    assert!(fixture.services.calls().is_empty());
    struct Cancel;
    impl parrot_domain::contracts::async_ops::Cancellation for Cancel {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    let write = snapshot.find("write").unwrap().create();
    let result = write
        .invoke(
            Invocation::new("cancel", r#"{"path":"old.txt","content":"new"}"#),
            parrot_domain::contracts::security::InvocationContext::new(
                support::Snapshot { id: 3 },
                catalog::Selection {
                    requested_model: "alias".into(),
                    scoped: support::Selection { id: 4 },
                },
                &Cancel,
            ),
        )
        .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert!(fixture.services.calls().is_empty());
    fixture.close().await;
}
