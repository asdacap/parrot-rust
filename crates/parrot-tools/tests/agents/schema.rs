use std::collections::BTreeMap;

use parrot_domain::contracts::tools::Tool;
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_tools::agents::{AgentTool, AgentToolKind};

#[test]
fn advertised_schemas_retain_original_fields_defaults_and_limits() {
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic/t58-home".into())]),
    )
    .unwrap();
    let schemas: BTreeMap<_, serde_json::Value> = [
        "agent_spawn",
        "agent_send",
        "agent_interrupt",
        "agent_status",
        "wait",
        "question",
        "ask",
        "reply",
    ]
    .into_iter()
    .map(|name| {
        (
            name,
            serde_json::from_str(&configuration.runtime.tools[name].parameters_json).unwrap(),
        )
    })
    .collect();
    for schema in schemas.values() {
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
    }
    assert_eq!(
        schemas["agent_spawn"]["required"],
        serde_json::json!(["prompt", "agent", "name"])
    );
    assert_eq!(
        schemas["agent_spawn"]["properties"]["fork"]["default"],
        "empty"
    );
    assert_eq!(
        schemas["agent_interrupt"]["required"],
        serde_json::json!(["name"])
    );
    assert!(
        schemas["agent_interrupt"]["properties"]["name"]
            .get("minLength")
            .is_none()
    );
    assert_eq!(
        schemas["wait"]["properties"]["duration_ms"]["type"],
        "integer"
    );
    assert_eq!(
        schemas["wait"]["properties"]["duration_ms"]["minimum"],
        10000
    );
    assert_eq!(
        schemas["wait"]["properties"]["duration_ms"]["maximum"],
        4294967294_u64
    );
    assert_eq!(
        schemas["wait"]["properties"]["duration_ms"]["default"],
        300000
    );
    assert_eq!(
        schemas["question"]["properties"]["questions"]["minItems"],
        1
    );
    assert_eq!(
        schemas["question"]["properties"]["questions"]["maxItems"],
        32
    );
    assert!(!configuration.runtime.tools.contains_key("answer"));
    assert_eq!(
        schemas["ask"]["required"],
        serde_json::json!(["name", "message"])
    );
    assert_eq!(
        schemas["reply"]["required"],
        serde_json::json!(["request_id", "message"])
    );
    for (tool, target) in [("ask", "name"), ("reply", "request_id")] {
        for field in [target, "message"] {
            assert_eq!(schemas[tool]["properties"][field]["type"], "string");
            assert_eq!(schemas[tool]["properties"][field]["minLength"], 1);
        }
    }
    let name_description = schemas["agent_send"]["properties"]["name"]["description"]
        .as_str()
        .unwrap();
    assert!(name_description.contains("bare 'parent'"));
    assert!(name_description.contains("parent/parent/sibling"));
    assert!(name_description.contains("any ancestor"));
}

#[test]
fn actual_template_catalog_adapter_preserves_formatter_and_send_factory_keys() {
    use parrot_tools::agents::{ResultTemplates, send_definition};
    let (configuration, templates) = PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic/t58-home".into())]),
    )
    .unwrap();
    assert_eq!(
        ResultTemplates::render(&templates, "tool-result.error", &[("message", "failure")])
            .unwrap(),
        "error: failure"
    );
    assert_eq!(
        ResultTemplates::render(
            &templates,
            "tool-result.question-answer",
            &[("question", "Pick"), ("answer", "yes")]
        )
        .unwrap(),
        "Question: Pick\nAnswer: yes"
    );
    let definition = &configuration.runtime.tools["agent_send"];
    let configured = parrot_domain::values::tools::ConfiguredToolDefinition::new(
        definition.description.clone(),
        definition.parameters_json.clone(),
    );
    let restricted = send_definition(configured.clone(), false, &templates).unwrap();
    assert_eq!(
        restricted.description,
        ResultTemplates::render(
            &templates,
            "agent-send-tool.description-without-parent",
            &[]
        )
        .unwrap()
    );
    assert_eq!(restricted.parameters_json, configured.parameters_json);
}

#[test]
fn tool_keeps_actual_configured_schema_and_description_without_reconstruction() {
    let (configuration, _) = PromptTemplateCatalog::from_content("tools:\n  agent_status:\n    description: Configured activity.\n    parameters:\n      properties:\n        name:\n          description: Configured target.\n", &BTreeMap::from([("HOME".into(), "/synthetic/t58-home".into())])).unwrap();
    let source = &configuration.runtime.tools["agent_status"];
    let configured = parrot_domain::values::tools::ConfiguredToolDefinition::new(
        source.description.clone(),
        source.parameters_json.clone(),
    );
    let tool = AgentTool::new(
        AgentToolKind::Status,
        configured.clone(),
        std::sync::Arc::new(super::tools::Runtime::default()),
        std::sync::Arc::new(super::tools::Models),
        None,
        false,
    );
    assert_eq!(tool.definition(), &configured);
    assert_eq!(tool.definition().description, "Configured activity.");
    let schema: serde_json::Value =
        serde_json::from_str(&tool.definition().parameters_json).unwrap();
    assert_eq!(
        schema["properties"]["name"]["description"],
        "Configured target."
    );
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert_eq!(schema["required"], serde_json::json!(["name"]));
}

#[test]
fn ask_guidance_and_results_are_configurable_with_explicit_request_ids() {
    use parrot_tools::agents::ResultTemplates;
    let (_, templates) = PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic/t58-home".into())]),
    )
    .unwrap();
    let request = ResultTemplates::render(
        &templates,
        "agent-session.agent-ask",
        &[
            ("asker_name", "child"),
            ("request_id", "req-1"),
            ("message", "Proceed?"),
        ],
    )
    .unwrap();
    assert!(request.contains("Agent child"));
    assert!(request.contains("req-1"));
    assert!(request.contains("Proceed?"));
    assert!(request.contains("Use reply"));
    let reminder = ResultTemplates::render(
        &templates,
        "agent-session.pending-ask-reminder",
        &[("requests", "\n- req-1 from child")],
    )
    .unwrap();
    assert!(reminder.contains("req-1 from child"));
    assert!(reminder.contains("Use reply"));
    assert_eq!(
        ResultTemplates::render(
            &templates,
            "tool-result.ask-reply",
            &[("message", "  yes\nnow  ")]
        )
        .unwrap(),
        "  yes\nnow  "
    );
    assert_eq!(
        ResultTemplates::render(
            &templates,
            "tool-result.ask-replied",
            &[("request_id", "req-1")]
        )
        .unwrap(),
        "Replied to ask req-1."
    );
}
