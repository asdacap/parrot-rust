//! Catalog/config integration: original-source bytes plus explicitly mapped A008 boundaries.
mod support;
use parrot_domain::contracts::async_ops::OperationError;
use parrot_foundation::{
    config::{Configuration, configuration::source_predefined_content},
    template_catalog::PromptTemplateCatalog,
};
use serde_json::json;
use std::collections::BTreeSet;
use support::*;

const A: &str = "tests/fixtures/scriban/a008/";
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
                &CancellationState::Active
            )
            .unwrap(),
        "Agent worker says:\nvalue {not_a_placeholder}\nnext"
    );
    let inherited = catalog
        .render(
            "tool-result.output-blob-notice",
            &arguments(&json!({"path":"/tmp/{runtime}"})),
            &CancellationState::Active,
        )
        .unwrap();
    assert!(inherited.contains("/tmp/{runtime}; metadata: {literal}"));
}

#[test]
fn m1_three_mixed_category_assertions_remain_scalar_and_exact() {
    let baseline = defaults();
    let content = "prompt_templates:\n  agent-task.stop-canceled:\n    template: Task canceled by owner.\n  agent-session.child-completion:\n    template: '{agent_name}: {result}'\nprofiles:\n  build:\n    usage: Custom foreground usage\nlive_buffer_rows: 37\nagent_tasks:\n  maximum_attempts: 7\n";
    let (_, catalog) = PromptTemplateCatalog::from_content(content, &environment()).unwrap();
    assert_eq!(
        catalog
            .render("agent-task.stop-canceled", &[], &CancellationState::Active)
            .unwrap(),
        "Task canceled by owner."
    );
    assert_eq!(
        catalog
            .render(
                "agent-session.child-completion",
                &arguments(&json!({"agent_name":"worker","result":"literal {value}"})),
                &CancellationState::Active
            )
            .unwrap(),
        "worker: literal {value}"
    );
    assert_eq!(
        catalog
            .render("agent-task.stop-succeeded", &[], &CancellationState::Active)
            .unwrap(),
        baseline
            .render("agent-task.stop-succeeded", &[], &CancellationState::Active)
            .unwrap()
    );
    assert_eq!(template_specs(content).len(), 110);
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
                    .render(
                        "custom.structured",
                        &arguments(&model),
                        &CancellationState::Active
                    )
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
            .render(
                "custom.structured",
                &arguments(&model),
                &CancellationState::Active
            )
            .unwrap(),
        "x;".repeat(1001)
    );
    assert_eq!(
        catalog.render(
            "custom.structured",
            &arguments(&model),
            &CancellationState::Cancelled
        ),
        Err(OperationError::Cancelled)
    );
    let separate = custom("{{ missing }}", &[], &[]);
    assert!(
        error(separate.render("custom.catalog", &[], &CancellationState::Active))
            .contains("undefined value")
    );
}

#[test]
fn m5_ordering_duplicate_extra_required_and_string_only_scalar() {
    let content = "prompt_templates:\n  custom.order:\n    template: '{z}:{a}'\n    allowed_arguments: [z, a, optional]\n    required_arguments: [z, a]\n";
    let (_, catalog) = PromptTemplateCatalog::from_content(content, &environment()).unwrap();
    let spec = template_specs(content)
        .into_iter()
        .find(|spec| spec.path == "prompt_templates.custom.order")
        .unwrap();
    assert_eq!(spec.allowed, ["z", "a", "optional"]);
    assert_eq!(spec.required, ["z", "a"]);
    assert_eq!(
        error(catalog.render("custom.order", &[], &CancellationState::Active)),
        "prompt_templates.custom.order requires argument 'z'"
    );
    let (_, reversed) = PromptTemplateCatalog::from_content(
        &content.replace("required_arguments: [z, a]", "required_arguments: [a, z]"),
        &environment(),
    )
    .unwrap();
    assert_eq!(
        error(reversed.render("custom.order", &[], &CancellationState::Active)),
        "prompt_templates.custom.order requires argument 'a'"
    );
    let duplicate = vec![("unknown".into(), json!(1)), ("unknown".into(), json!(2))];
    assert_eq!(
        error(catalog.render("custom.order", &duplicate, &CancellationState::Active)),
        "prompt_templates.custom.order received duplicate argument 'unknown'"
    );
    assert_eq!(
        error(catalog.render("absent", &duplicate, &CancellationState::Active)),
        "prompt_templates.absent received duplicate argument 'unknown'"
    );
    assert_eq!(
        error(catalog.render(
            "custom.order",
            &[("x".into(), json!(1)), ("b".into(), json!(2))],
            &CancellationState::Active
        )),
        "prompt_templates.custom.order does not allow argument 'x'"
    );
    assert_eq!(
        error(catalog.render(
            "custom.order",
            &arguments(&json!({"z":null,"a":"x"})),
            &CancellationState::Active
        )),
        "prompt_templates.custom.order requires scalar argument 'z'"
    );
    for value in [json!(true), json!(42), json!(1.5), json!([]), json!({})] {
        assert_eq!(
            error(catalog.render(
                "custom.order",
                &[("z".into(), value), ("a".into(), json!("a"))],
                &CancellationState::Active
            )),
            "prompt_templates.custom.order requires scalar argument 'z'"
        );
    }
    assert_eq!(
        catalog
            .render(
                "custom.order",
                &arguments(&json!({"z":"${NAME} {{ hostile }}\n🪶","a":"{z}","optional":{}})),
                &CancellationState::Active
            )
            .unwrap(),
        "${NAME} {{ hostile }}\n🪶:{z}"
    );
    assert_eq!(
        error(catalog.render("absent", &[], &CancellationState::Active)),
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
                    .render("custom.catalog", &pairs, &CancellationState::Active)
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
            &CancellationState::Active
        ))
        .contains("not callable")
    );
}

#[test]
fn m7_precancelled_catalog_rejects_validation_and_rendering_errors() {
    let catalog = defaults();
    assert_eq!(
        catalog.render(
            "absent",
            &[("x".into(), json!({})), ("x".into(), json!({}))],
            &CancellationState::Cancelled
        ),
        Err(OperationError::Cancelled)
    );
    let native = custom("{{ missing }}", &[], &[]);
    assert_eq!(
        native.render("custom.catalog", &[], &CancellationState::Cancelled),
        Err(OperationError::Cancelled)
    );
    assert_eq!(
        catalog.render(
            "system.working-directory",
            &arguments(&json!({"working_directory":{}})),
            &CancellationState::Cancelled
        ),
        Err(OperationError::Cancelled)
    );
}

#[test]
fn m7_native_complete_output_empty_output_and_bounded_failures_through_catalog() {
    let output = custom("prefix{{ value }}suffix", &["value"], &["value"]);
    assert_eq!(
        output
            .render(
                "custom.catalog",
                &arguments(&json!({"value":"data"})),
                &CancellationState::Active
            )
            .unwrap(),
        "prefixdatasuffix"
    );
    let fuel = custom(NO_OUTPUT_FUEL, &[], &[]);
    for _ in 0..2 {
        let message = error(fuel.render("custom.catalog", &[], &CancellationState::Active));
        assert!(
            message.starts_with(&format!("{PATH}.template:"))
                && message.contains("engine ran out of fuel"),
            "{message}"
        );
    }
    let completion = custom(
        "{% for i in range(1001) %}{% set x=i %}{% endfor %}",
        &[],
        &[],
    );
    assert_eq!(
        completion
            .render("custom.catalog", &[], &CancellationState::Active)
            .unwrap(),
        ""
    );
    let failure = custom(
        "prefix{% for i in range(1001) %}{% for j in range(1001) %}x{% endfor %}{% endfor %}",
        &[],
        &[],
    );
    assert!(
        error(failure.render("custom.catalog", &[], &CancellationState::Active))
            .contains("engine ran out of fuel")
    );
    let recursion = custom(
        "prefix{% macro recur() %}{{ recur() }}{% endmacro %}{{ recur() }}",
        &[],
        &[],
    );
    assert!(
        error(recursion.render("custom.catalog", &[], &CancellationState::Active))
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
        error(custom(&text, &[], &[]).render("custom.catalog", &[], &CancellationState::Active))
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
            error(custom(text, &[], &[]).render("custom.catalog", &[], &CancellationState::Active));
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
    let (_, catalog) = PromptTemplateCatalog::load(&path, &predefined, &env).unwrap();
    let mut loaded = 0;
    Configuration::load(&path, &predefined, &env, |_| {
        loaded += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(loaded, 111);
    assert_eq!(
        catalog
            .render(
                "custom.structured",
                &arguments(&json!({"value":"{{ literal }}"})),
                &CancellationState::Active
            )
            .unwrap(),
        "{{ literal }}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
    assert_eq!(
        std::fs::read_to_string(&predefined).unwrap(),
        parrot_foundation::config::yaml::compose_with_end(
            &parrot_foundation::config::configuration::predefined_content().unwrap()
        )
    );
    let active =
        parrot_foundation::config::yaml::compose_with_end(&source_predefined_content().unwrap());
    assert!(active.contains("engine: minijinja"));
    assert!(!active.contains("engine: scriban"));
    std::fs::remove_dir_all(directory).unwrap();
}
