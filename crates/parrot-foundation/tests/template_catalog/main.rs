//! Catalog/config integration: original-source bytes plus explicitly mapped A008 boundaries.
#[path = "../config/product_changes.rs"]
mod product_changes;
mod support;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_foundation::{
    config::configuration::source_predefined_content, template_catalog::PromptTemplateCatalog,
};
use serde_json::json;
use std::collections::BTreeSet;
use support::*;

const A: &str = "tests/fixtures/scriban/a008/";
const H: &str = "crates/parrot-foundation/tests/template_migration/fixtures/";
const PATH: &str = "prompt_templates.custom.catalog";
const NO_OUTPUT_FUEL: &str =
    "{% for i in range(1001) %}{% for j in range(1001) %}{% set x=j %}{% endfor %}{% endfor %}";

fn defaults() -> PromptTemplateCatalog {
    PromptTemplateCatalog::from_content("", &environment())
        .unwrap()
        .1
}
fn custom(text: &str, allowed: &[&str], required: &[&str]) -> PromptTemplateCatalog {
    PromptTemplateCatalog::from_content(&native_yaml(text, allowed, required), &environment())
        .unwrap()
        .1
}
fn config_error(content: &str) -> String {
    match PromptTemplateCatalog::from_content(content, &environment()) {
        Err(error) => error.0,
        Ok(_) => panic!("expected real configuration/engine validation failure"),
    }
}

const CONSOLIDATED_SOURCE_IDS: [&str; 13] = [
    "agent-task.dependency-header",
    "agent-task.dependency-item",
    "agent-task.sibling-scope",
    "agent-task.sibling-item",
    "agent-task.header",
    "agent-session.nonempty-queues-reminder",
    "agent-session.nonempty-queue-item",
    "compact-context-tool.compacted",
    "compact-context-tool.no-op",
    "compact-context-tool.unavailable",
    "compact-context-tool.compacted-with-limit",
    "compact-context-tool.no-op-with-limit",
    "compact-context-tool.unavailable-with-limit",
];

const AGENT_COMMUNICATION_SOURCE_IDS: [&str; 5] = [
    "agent-session.child-question",
    "agent-session.pending-child-question-reminder",
    "tool-result.question-replied",
    "agent-session.final-request",
    "agent-session.final-request-interrupted",
];

#[test]
fn m6_full_configuration_compiles_and_renders_retained_independent_source_models() {
    let (configuration, catalog) = PromptTemplateCatalog::from_content("", &environment()).unwrap();
    assert_eq!(configuration.prompt_templates.len(), 99);
    let inventory = fixture(&(A.to_owned() + "catalog-inventory.json"));
    let migration = fixture(&(A.to_owned() + "bundled-migration-map.json"));
    let actual: Vec<_> = catalog.descriptors().collect();
    assert_eq!(actual.len(), 99);
    let (_, legacy_warning) = actual
        .iter()
        .find(|(id, _)| *id == "session.legacy-resume-warning")
        .unwrap();
    assert_eq!(legacy_warning.engine, "scalar");
    assert!(legacy_warning.allowed.is_empty());
    assert!(legacy_warning.required.is_empty());
    assert_eq!(
        catalog
            .render("session.legacy-resume-warning", &[], &CancelAt::never())
            .unwrap(),
        legacy_warning.text
    );
    let expected = inventory["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            !CONSOLIDATED_SOURCE_IDS.contains(&row["id"].as_str().unwrap())
                && !AGENT_COMMUNICATION_SOURCE_IDS.contains(&row["id"].as_str().unwrap())
        })
        .collect::<Vec<_>>();
    let expected_ids = expected
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    let actual = actual
        .into_iter()
        .filter(|(id, _)| expected_ids.contains(id))
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), 89);
    for ((id, spec), original) in actual.iter().zip(expected) {
        assert_eq!(*id, original["id"].as_str().unwrap());
        assert_eq!(
            spec.allowed,
            serde_json::from_value::<Vec<String>>(original["allowed"].clone()).unwrap()
        );
        assert_eq!(
            spec.required,
            serde_json::from_value::<Vec<String>>(original["required"].clone()).unwrap()
        );
        if original["engine"] == "scalar" {
            assert_eq!(spec.engine, "scalar");
            assert_eq!(
                spec.text,
                product_changes::align_product_text(id, original["text"].as_str().unwrap())
            );
        } else {
            let mapped = migration["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == *id)
                .unwrap();
            assert_eq!(spec.engine, "minijinja");
            assert_eq!(spec.text, mapped["new"]["text"].as_str().unwrap());
        }
    }
    let goldens = fixture(&(A.to_owned() + "catalog-render-goldens.json"));
    assert_eq!(goldens["entries"].as_array().unwrap().len(), 107);
    for row in goldens["entries"].as_array().unwrap() {
        if CONSOLIDATED_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
            || AGENT_COMMUNICATION_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
        {
            continue;
        }
        let rendered = catalog
            .render(
                row["template_id"].as_str().unwrap(),
                &arguments(&row["arguments"]),
                &CancelAt::never(),
            )
            .unwrap();
        assert_eq!(
            rendered,
            product_changes::align_product_text(
                row["template_id"].as_str().unwrap(),
                row["expected"].as_str().unwrap(),
            ),
            "{}",
            row["case_id"]
        );
    }
}

#[test]
fn m2_all_ten_templates_thirty_empty_multiple_nested_cases_use_real_catalog() {
    let catalog = defaults();
    let inputs = fixture(&(H.to_owned() + "m2-source-input.json"));
    let expected = fixture(&(H.to_owned() + "m2-source-oracle.json"));
    let mut variants = std::collections::BTreeMap::<&str, BTreeSet<&str>>::new();
    assert_eq!(inputs.as_array().unwrap().len(), 30);
    for (input, result) in inputs
        .as_array()
        .unwrap()
        .iter()
        .zip(expected["cases"].as_array().unwrap())
    {
        assert_eq!(input["id"], result["id"]);
        let id = input["template_id"].as_str().unwrap();
        variants.entry(id).or_default().insert(
            input["id"]
                .as_str()
                .unwrap()
                .strip_prefix(&format!("A008.M2.{id}."))
                .unwrap(),
        );
        assert_eq!(
            catalog
                .render(id, &arguments(&input["arguments"]), &CancelAt::never())
                .unwrap(),
            result["output"]["value"].as_str().unwrap(),
            "{}",
            input["id"]
        );
    }
    assert_eq!(variants.len(), 10);
    assert!(
        variants
            .values()
            .all(|v| v == &BTreeSet::from(["empty", "multiple", "nested-branch"]))
    );
}

#[test]
fn m1_all_fourteen_canonical_rows_and_twenty_seven_mapped_assertion_probes() {
    let mapping = fixture(&(A.to_owned() + "m1-migration-map.json"));
    let rows = mapping["entries"].as_array().unwrap();
    assert_eq!(rows.len(), 14);
    assert_eq!(
        rows.iter()
            .map(|r| r["source_method"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        7
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["T11_handoff_disposition"] == "delegated_T12_not_executed")
            .count(),
        11
    );
    let mut ids = BTreeSet::new();
    let mut boundaries = BTreeSet::new();
    for row in rows {
        for case in row["assertion_migrations"].as_array().unwrap() {
            ids.insert(case["source_case_id"].as_str().unwrap());
            let expected = &case["new_expected"];
            if expected["api_boundary"] == "owned-serde-only" {
                // These three source Version(1,0) assertions are not fake successful renders.
                // The catalog's compile_fail doctest proves host/engine values cannot enter.
                assert_eq!(
                    case["new_input"]["probe_kind"],
                    "owned-serde-api-boundary-not-a-render-case"
                );
                assert_eq!(expected["host_objects_representable"], false);
                boundaries.insert(case["source_case_id"].as_str().unwrap());
                continue;
            }
            let actual = probe(&case["new_input"]);
            if let Some(value) = expected["value"].as_str() {
                assert_eq!(actual.unwrap(), value, "{}", case["source_case_id"]);
            } else if expected["cancelled"] == true {
                assert_eq!(actual, Err(OperationError::Cancelled));
            } else {
                let message = error(actual);
                if let Some(exact) = expected["error"].as_str() {
                    assert_eq!(message, exact, "{}", case["source_case_id"]);
                } else {
                    assert!(
                        message.starts_with(expected["error_prefix"].as_str().unwrap()),
                        "{message}"
                    );
                    match expected["error_kind"].as_str() {
                        Some("UndefinedError") => {
                            assert!(message.contains("undefined value"), "{message}")
                        }
                        Some("SyntaxError") => {
                            assert!(message.contains("syntax error"), "{message}")
                        }
                        _ => {
                            for engine in expected["valid_engine_names"].as_array().unwrap() {
                                assert!(message.contains(engine.as_str().unwrap()), "{message}");
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(ids.len(), 27);
    assert_eq!(boundaries.len(), 3);
}

#[test]
fn m1_scalar_both_original_assertions_share_the_overridden_configuration() {
    let inputs = fixture(&(A.to_owned() + "source-input.json"));
    let input = inputs
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "A008.M1.scalar.multiline")
        .unwrap();
    let (_, catalog) =
        PromptTemplateCatalog::from_content(input["yaml"].as_str().unwrap(), &environment())
            .unwrap();
    assert_eq!(
        catalog
            .render(
                "agent-session.child-completion",
                &arguments(&input["arguments"]),
                &CancelAt::never()
            )
            .unwrap(),
        "Agent worker says:\nvalue {not_a_placeholder}\nnext"
    );
    let inherited = catalog
        .render(
            "tool-result.output-blob-notice",
            &arguments(&json!({"path":"/tmp/{runtime}"})),
            &CancelAt::never(),
        )
        .unwrap();
    assert!(inherited.contains("/tmp/{runtime}; metadata: {literal}"));
}

#[test]
fn m1_three_mixed_category_assertions_remain_scalar_and_exact() {
    let baseline = defaults();
    let content = "prompt_templates:\n  agent-task.stop-canceled:\n    template: Task canceled by owner.\n  agent-session.child-completion:\n    template: '{agent_name}: {result}'\nprofiles:\n  build:\n    usage: Custom foreground usage\nlive_buffer_rows: 37\nagent_tasks:\n  maximum_attempts: 7\n";
    let (config, catalog) = PromptTemplateCatalog::from_content(content, &environment()).unwrap();
    assert_eq!(
        catalog
            .render("agent-task.stop-canceled", &[], &CancelAt::never())
            .unwrap(),
        "Task canceled by owner."
    );
    assert_eq!(
        catalog
            .render(
                "agent-session.child-completion",
                &arguments(&json!({"agent_name":"worker","result":"literal {value}"})),
                &CancelAt::never()
            )
            .unwrap(),
        "worker: literal {value}"
    );
    assert_eq!(
        catalog
            .render("agent-task.stop-succeeded", &[], &CancelAt::never())
            .unwrap(),
        baseline
            .render("agent-task.stop-succeeded", &[], &CancelAt::never())
            .unwrap()
    );
    assert_eq!(config.prompt_templates.len(), 99);
}

#[test]
fn m1_m3_m6_repeat_calls_large_loop_and_inputs_are_isolated() {
    let map = fixture(&(A.to_owned() + "m1-migration-map.json"));
    let case = map["entries"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["assertion_migrations"].as_array().unwrap())
        .find(|case| case["source_case_id"] == "A008.M1.custom.true.repeat1")
        .unwrap();
    let content = case["new_input"]["yaml"].as_str().unwrap();
    let (_, catalog) = PromptTemplateCatalog::from_content(content, &environment()).unwrap();
    let mut model = case["new_input"]["arguments"].clone();
    let saved = model.clone();
    for visible in [true, false, true, false] {
        model["visible"] = json!(visible);
        let expected = if visible {
            "{{ hostile }} {value}\nnext;{{ hostile }} {value}\nnext;"
        } else {
            "hidden"
        };
        for _ in 0..2 {
            assert_eq!(
                catalog
                    .render("custom.structured", &arguments(&model), &CancelAt::never())
                    .unwrap(),
                expected
            );
        }
    }
    model["visible"] = json!(true);
    assert_eq!(model, saved);
    model["items"] = json!(vec![json!({"name":"x"}); 1001]);
    assert_eq!(
        catalog
            .render("custom.structured", &arguments(&model), &CancelAt::never())
            .unwrap(),
        "x;".repeat(1001)
    );
    assert_eq!(
        catalog.render("custom.structured", &arguments(&model), &CancelAt::new(1)),
        Err(OperationError::Cancelled)
    );
    let separate = custom("{{ missing }}", &[], &[]);
    assert!(
        error(separate.render("custom.catalog", &[], &CancelAt::never()))
            .contains("undefined value")
    );
}

#[test]
fn m5_ordering_duplicate_extra_required_and_string_only_scalar() {
    let content = "prompt_templates:\n  custom.order:\n    template: '{z}:{a}'\n    allowed_arguments: [z, a, optional]\n    required_arguments: [z, a]\n";
    let (_, catalog) = PromptTemplateCatalog::from_content(content, &environment()).unwrap();
    let spec = catalog
        .descriptors()
        .find(|(id, _)| *id == "custom.order")
        .unwrap()
        .1;
    assert_eq!(spec.allowed, ["z", "a", "optional"]);
    assert_eq!(spec.required, ["z", "a"]);
    assert_eq!(
        error(catalog.render("custom.order", &[], &CancelAt::never())),
        "prompt_templates.custom.order requires argument 'z'"
    );
    let (_, reversed) = PromptTemplateCatalog::from_content(
        &content.replace("required_arguments: [z, a]", "required_arguments: [a, z]"),
        &environment(),
    )
    .unwrap();
    assert_eq!(
        error(reversed.render("custom.order", &[], &CancelAt::never())),
        "prompt_templates.custom.order requires argument 'a'"
    );
    let duplicate = vec![("unknown".into(), json!(1)), ("unknown".into(), json!(2))];
    assert_eq!(
        error(catalog.render("custom.order", &duplicate, &CancelAt::never())),
        "prompt_templates.custom.order received duplicate argument 'unknown'"
    );
    assert_eq!(
        error(catalog.render("absent", &duplicate, &CancelAt::never())),
        "prompt_templates.absent received duplicate argument 'unknown'"
    );
    assert_eq!(
        error(catalog.render(
            "custom.order",
            &[("x".into(), json!(1)), ("b".into(), json!(2))],
            &CancelAt::never()
        )),
        "prompt_templates.custom.order does not allow argument 'x'"
    );
    assert_eq!(
        error(catalog.render(
            "custom.order",
            &arguments(&json!({"z":null,"a":"x"})),
            &CancelAt::never()
        )),
        "prompt_templates.custom.order requires scalar argument 'z'"
    );
    for value in [json!(true), json!(42), json!(1.5), json!([]), json!({})] {
        assert_eq!(
            error(catalog.render(
                "custom.order",
                &[("z".into(), value), ("a".into(), json!("a"))],
                &CancelAt::never()
            )),
            "prompt_templates.custom.order requires scalar argument 'z'"
        );
    }
    assert_eq!(
        catalog
            .render(
                "custom.order",
                &arguments(&json!({"z":"${NAME} {{ hostile }}\n🪶","a":"{z}","optional":{}})),
                &CancelAt::never()
            )
            .unwrap(),
        "${NAME} {{ hostile }}\n🪶:{z}"
    );
    assert_eq!(
        error(catalog.render("absent", &[], &CancelAt::never())),
        "prompt_templates.absent is not defined"
    );
}

#[test]
fn m5_real_callback_rejects_invalid_scalar_and_native_compile_not_fixture_admission() {
    for (text, reason) in [
        ("{unknown}", "unknown placeholder"),
        ("{path} {path}", "duplicate placeholder"),
        ("{path", "unmatched opening brace"),
        ("x}", "unmatched closing brace"),
    ] {
        let content = format!(
            "prompt_templates:\n  tool-result.output-blob-notice:\n    template: {}\n",
            serde_json::to_string(text).unwrap()
        );
        let message = config_error(&content);
        assert!(message.starts_with("prompt_templates.tool-result.output-blob-notice.template"));
        assert!(message.contains(reason), "{message}");
    }
    let message = config_error(&native_yaml("{% for x in values %}", &[], &[]));
    assert!(
        message.starts_with(&format!("{PATH}.template:")) && message.contains("syntax error"),
        "{message}"
    );
}

#[test]
fn m3_m8_unicode_opaque_native_content_scope_and_data_are_not_callables() {
    let content = "{{ value }}{% set value='changed' %}|${NAME}";
    let catalog = custom(content, &["value"], &["value"]);
    for value in ["🪶 é \r\n{{ hostile }} {literal}", "second"] {
        let pairs = vec![("value".into(), json!(value))];
        for _ in 0..2 {
            assert_eq!(
                catalog
                    .render("custom.catalog", &pairs, &CancelAt::never())
                    .unwrap(),
                format!("{value}|${{NAME}}")
            );
        }
    }
    let callable = custom("{{ callable() }}", &["callable"], &["callable"]);
    assert!(
        error(callable.render(
            "custom.catalog",
            &arguments(&json!({"callable":{"name":"exec"}})),
            &CancelAt::never()
        ))
        .contains("not callable")
    );
}

#[test]
fn m7_catalog_cancels_before_validation_and_before_any_value_conversion() {
    let catalog = defaults();
    let token = CancelAt::new(1);
    assert_eq!(
        catalog.render(
            "absent",
            &[("x".into(), json!({})), ("x".into(), json!({}))],
            &token
        ),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 1);
    let native = custom("{{ missing }}", &[], &[]);
    for at in [2, 3, 4, 5] {
        let token = CancelAt::new(at);
        assert_eq!(
            native.render("custom.catalog", &[], &token),
            Err(OperationError::Cancelled)
        );
        assert_eq!(token.calls(), at);
    }
    let scalar = defaults();
    let token = CancelAt::new(2);
    assert_eq!(
        scalar.render(
            "system.working-directory",
            &arguments(&json!({"working_directory":{}})),
            &token
        ),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 2);
}

#[test]
fn m7_native_output_no_output_fuel_recursion_and_cancellation_precedence_through_catalog() {
    let output = custom("prefix{{ value }}suffix", &["value"], &["value"]);
    let token = CancelAt::new(6); // catalog2 + native2 + writes; post-native check observes again.
    assert_eq!(
        output.render(
            "custom.catalog",
            &arguments(&json!({"value":"data"})),
            &token
        ),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 7);
    let fuel = custom(NO_OUTPUT_FUEL, &[], &[]);
    for _ in 0..2 {
        let message = error(fuel.render("custom.catalog", &[], &CancelAt::never()));
        assert!(
            message.starts_with(&format!("{PATH}.template:"))
                && message.contains("engine ran out of fuel"),
            "{message}"
        );
    }
    let token = CancelAt::new(5); // no polling within native no-output work; observed only on return.
    assert_eq!(
        fuel.render("custom.catalog", &[], &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 5);
    let completion = custom(
        "{% for i in range(1001) %}{% set x=i %}{% endfor %}",
        &[],
        &[],
    );
    assert_eq!(
        completion.render("custom.catalog", &[], &CancelAt::new(5)),
        Err(OperationError::Cancelled)
    );
    assert_eq!(
        completion
            .render("custom.catalog", &[], &CancelAt::never())
            .unwrap(),
        ""
    );
    let failure = custom(
        "prefix{% for i in range(1001) %}{% for j in range(1001) %}x{% endfor %}{% endfor %}",
        &[],
        &[],
    );
    assert!(
        error(failure.render("custom.catalog", &[], &CancelAt::never()))
            .contains("engine ran out of fuel")
    );
    let recursion = custom(
        "prefix{% macro recur() %}{{ recur() }}{% endmacro %}{{ recur() }}",
        &[],
        &[],
    );
    assert!(
        error(recursion.render("custom.catalog", &[], &CancelAt::never()))
            .contains("recursion limit exceeded")
    );
}

#[test]
fn m8_no_loader_ambient_functions_or_scriban_aliases_in_catalog() {
    let directory = std::env::temp_dir().join(format!("parrot-t12-loader-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("private.txt");
    std::fs::write(&path, "NOT-ACCESSIBLE").unwrap();
    let text = format!(
        "{{% include {} %}}",
        serde_json::to_string(&path.to_string_lossy()).unwrap()
    );
    assert!(
        error(custom(&text, &[], &[]).render("custom.catalog", &[], &CancelAt::never()))
            .contains("template not found")
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
    for text in [
        "{{ env('HOME') }}",
        "{{ getenv('HOME') }}",
        "{{ exec('id') }}",
        "{{ http_get('http://127.0.0.1/') }}",
        "{{ process }}",
        "{{ network }}",
        "{{ string.upcase('x') }}",
        "{{ date.now }}",
    ] {
        let message =
            error(custom(text, &[], &[]).render("custom.catalog", &[], &CancelAt::never()));
        assert!(
            message.starts_with(&format!("{PATH}.template:")),
            "{text}: {message}"
        );
    }
}

#[test]
fn m9_explicit_scriban_is_path_specific_manual_migration_without_rewriting_user_file() {
    let directory = std::env::temp_dir().join(format!("parrot-t12-config-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.yaml");
    let predefined = directory.join("predefined.yaml");
    let original = "prompt_templates:\n  custom.structured:\n    engine: scriban\n    template: '{{ for item in items }}'\n    allowed_arguments: []\n    required_arguments: []\n";
    std::fs::write(&path, original).unwrap();
    let env: std::collections::BTreeMap<_, _> = [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
        ("WORKDIR", "work"),
    ]
    .into_iter()
    .map(|(key, child)| {
        (
            key.to_owned(),
            directory.join(child).to_string_lossy().into_owned(),
        )
    })
    .collect();
    let message = match PromptTemplateCatalog::load(&path, &predefined, &env) {
        Err(error) => error.0,
        Ok(_) => panic!("must reject historical engine"),
    };
    assert!(message.starts_with("prompt_templates.custom.structured.engine:"));
    assert!(
        message.contains("manually migrate")
            && message.contains("engine: minijinja")
            && message.contains("template-migration-guide.md")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let new = "prompt_templates:\n  custom.structured:\n    engine: minijinja\n    template: '{{ value }}'\n    allowed_arguments: [value]\n    required_arguments: [value]\n";
    std::fs::write(&path, new).unwrap();
    let (config, catalog) = PromptTemplateCatalog::load(&path, &predefined, &env).unwrap();
    assert_eq!(config.prompt_templates.len(), 100);
    assert_eq!(
        catalog
            .render(
                "custom.structured",
                &arguments(&json!({"value":"{{ literal }}"})),
                &CancelAt::never()
            )
            .unwrap(),
        "{{ literal }}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
    assert_eq!(
        std::fs::read_to_string(&predefined).unwrap(),
        parrot_foundation::config::configuration::predefined_content().unwrap()
    );
    let goldens = fixture(&(A.to_owned() + "catalog-render-goldens.json"));
    for row in goldens["entries"].as_array().unwrap() {
        if CONSOLIDATED_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
            || AGENT_COMMUNICATION_SOURCE_IDS.contains(&row["template_id"].as_str().unwrap())
        {
            continue;
        }
        assert_eq!(
            catalog
                .render(
                    row["template_id"].as_str().unwrap(),
                    &arguments(&row["arguments"]),
                    &CancelAt::never()
                )
                .unwrap(),
            product_changes::align_product_text(
                row["template_id"].as_str().unwrap(),
                row["expected"].as_str().unwrap(),
            )
        );
    }
    let active = source_predefined_content().unwrap();
    assert!(active.contains("engine: minijinja"));
    assert!(!active.contains("engine: scriban"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn source_skill_catalog_conveniences_preserve_opaque_literal_arguments() {
    let catalog = defaults();
    let token = CancelAt::never();
    let hostile = "{{ value }}\n${NAME}";
    assert_eq!(
        catalog.render_skills(hostile, &token).unwrap(),
        catalog
            .render(
                "context.skills",
                &arguments(&json!({"skills":hostile})),
                &token
            )
            .unwrap()
    );
    assert_eq!(
        catalog
            .render_selected_skill("n", "/tmp/{x}", hostile, &token)
            .unwrap(),
        catalog
            .render(
                "context.skill-selected",
                &arguments(&json!({"name":"n","path":"/tmp/{x}","content":hostile})),
                &token
            )
            .unwrap()
    );
    assert_eq!(
        catalog
            .render_unavailable_skill("n", hostile, &token)
            .unwrap(),
        catalog
            .render(
                "context.skill-unavailable",
                &arguments(&json!({"name":"n","message":hostile})),
                &token
            )
            .unwrap()
    );
}
