use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::config::ConfigError;
use parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine;
use serde_json::{Map, Value, json};

const PATH: &str = "prompt_templates.custom.adapter";
// Each native range stays below its separate size guard; nested work exhausts fuel.
const NO_OUTPUT_FUEL: &str =
    "{% for i in range(1001) %}{% for j in range(1001) %}{% set x = j %}{% endfor %}{% endfor %}";

enum CancellationState {
    Active,
    Cancelled,
}

impl Cancellation for CancellationState {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}

fn arguments(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn engine(text: &str) -> MiniJinjaPromptTemplateEngine {
    MiniJinjaPromptTemplateEngine::compile(PATH, text).unwrap()
}

fn native_error(result: Result<String, OperationError<ConfigError>>) -> String {
    match result {
        Err(OperationError::Failed(error)) => {
            assert!(error.0.starts_with(&format!("{PATH}.template:")), "{error}");
            error.0
        }
        other => panic!("expected path-prefixed native error, got {other:?}"),
    }
}

#[test]
fn m3_m8_native_content_cases_preserve_independent_expected_bytes() {
    let cases: Value = serde_json::from_str(include_str!("cases.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let compiled = engine(case["template"].as_str().unwrap());
        let values = case["arguments"].as_object().unwrap();
        assert_eq!(
            compiled.render(values, &CancellationState::Active).unwrap(),
            case["expected"].as_str().unwrap(),
            "{}",
            case["id"]
        );
    }
}

#[test]
fn m3_calls_and_templates_do_not_leak_scope_or_mutate_inputs() {
    let compiled = engine("{{ value }}{% set value = 'changed' %}");
    let first = arguments(json!({"value":"first"}));
    let second = arguments(json!({"value":"second"}));
    let saved = first.clone();
    for _ in 0..3 {
        assert_eq!(
            compiled.render(&first, &CancellationState::Active).unwrap(),
            "first"
        );
        assert_eq!(
            compiled
                .render(&second, &CancellationState::Active)
                .unwrap(),
            "second"
        );
    }
    assert_eq!(first, saved);
    let other = engine("{{ value }}");
    native_error(other.render(&Map::new(), &CancellationState::Active));
    assert_eq!(
        other.render(&first, &CancellationState::Active).unwrap(),
        "first"
    );
}

#[test]
fn m4_missing_variable_is_strict_and_discards_prior_output() {
    let error = native_error(
        engine("PRIVATE-PARTIAL{{ missing }}").render(&Map::new(), &CancellationState::Active),
    );
    assert!(error.contains("undefined"), "{error}");
    assert!(
        !error.contains("PRIVATE-PARTIAL"),
        "debug source context must be disabled"
    );
}

#[test]
fn m4_missing_member_is_strict() {
    let error = native_error(
        engine("{{ item.missing }}")
            .render(&arguments(json!({"item":{}})), &CancellationState::Active),
    );
    assert!(error.contains("undefined"), "{error}");
}

#[test]
fn m4_missing_index_is_strict() {
    let error = native_error(engine("{{ items[1] }}").render(
        &arguments(json!({"items":[{}]})),
        &CancellationState::Active,
    ));
    assert!(error.contains("undefined"), "{error}");
}

#[test]
fn compile_reports_native_syntax_error_with_configuration_path() {
    let error = MiniJinjaPromptTemplateEngine::compile(PATH, "{% for item in items %}")
        .err()
        .unwrap();
    assert!(error.0.starts_with(&format!("{PATH}.template:")));
    assert!(error.0.contains("syntax error"), "{error}");
}

#[test]
fn m9_adapter_is_native_only_not_a_scriban_alias() {
    // The engine selector/manual-migration error is T11's configuration seam.
    let error = MiniJinjaPromptTemplateEngine::compile(PATH, "{{ if visible }}yes{{ end }}")
        .err()
        .unwrap();
    assert!(error.0.starts_with(&format!("{PATH}.template:")));
    assert!(error.0.contains("syntax error"), "{error}");
}

#[test]
fn m6_source_mapped_1001_items_fit_fuel_and_repeat_cleanly() {
    let mapping: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/scriban/a008/m1-migration-map.json"
    ))
    .unwrap();
    let case = mapping["entries"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["assertion_migrations"].as_array().unwrap())
        .find(|row| row["source_case_id"] == "A008.M1.custom.1001")
        .unwrap();
    let compiled = engine(
        "{% if visible %}{% for item in items %}{{ item.name }};{% endfor %}{% else %}hidden{% endif %}",
    );
    let values = case["new_input"]["arguments"].as_object().unwrap();
    for _ in 0..2 {
        assert_eq!(
            compiled.render(values, &CancellationState::Active).unwrap(),
            case["old_expected"]["value"].as_str().unwrap()
        );
    }
}

#[test]
fn m7_precancelled_render_rejects_success_and_native_errors() {
    let values = arguments(json!({"value":"data"}));
    for text in ["prefix{{ value }}suffix", "{{ missing }}", NO_OUTPUT_FUEL] {
        assert_eq!(
            engine(text).render(&values, &CancellationState::Cancelled),
            Err(OperationError::Cancelled)
        );
    }
}

#[test]
fn m7_active_render_returns_complete_output_and_empty_output() {
    assert_eq!(
        engine("prefix{{ value }}suffix")
            .render(
                &arguments(json!({"value":"data"})),
                &CancellationState::Active
            )
            .unwrap(),
        "prefixdatasuffix"
    );
    assert_eq!(
        engine("{% for i in range(1001) %}{% set x = i %}{% endfor %}")
            .render(&Map::new(), &CancellationState::Active)
            .unwrap(),
        ""
    );
}

#[test]
fn m7_no_output_fuel_failure_is_path_prefixed_and_resets_per_call() {
    let compiled = engine(NO_OUTPUT_FUEL);
    for _ in 0..2 {
        let error = native_error(compiled.render(&Map::new(), &CancellationState::Active));
        assert!(error.contains("engine ran out of fuel"), "{error}");
    }
}

#[test]
fn m7_fuel_failure_after_output_exposes_no_partial_result() {
    let compiled = engine(
        "prefix{% for i in range(1001) %}{% for j in range(1001) %}x{% endfor %}{% endfor %}",
    );
    let error = native_error(compiled.render(&Map::new(), &CancellationState::Active));
    assert!(error.contains("engine ran out of fuel"), "{error}");
}

#[test]
fn m7_native_recursion_failure_is_bounded_and_no_partial_escapes() {
    let compiled = engine("prefix{% macro recur() %}{{ recur() }}{% endmacro %}{{ recur() }}");
    let error = native_error(compiled.render(&Map::new(), &CancellationState::Active));
    assert!(error.contains("recursion limit exceeded"), "{error}");
}

#[test]
fn m8_no_filesystem_loader_even_for_existing_file() {
    let directory = std::env::temp_dir().join(format!("parrot-t12g-loader-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("private-template.txt");
    std::fs::write(&file, "MUST-NOT-BE-LOADED").unwrap();
    let compiled = engine(&format!(
        "{{% include {} %}}",
        serde_json::to_string(&file.to_string_lossy()).unwrap()
    ));
    let error = native_error(compiled.render(&Map::new(), &CancellationState::Active));
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(directory).unwrap();
    assert!(error.contains("template not found"), "{error}");
    assert!(!error.contains("MUST-NOT-BE-LOADED"));
}

#[test]
fn m8_no_ambient_environment_process_network_or_scriban_builtins() {
    for (text, category) in [
        ("{{ env('HOME') }}", "unknown function"),
        ("{{ getenv('HOME') }}", "unknown function"),
        ("{{ exec('id') }}", "unknown function"),
        ("{{ http_get('http://127.0.0.1/') }}", "unknown function"),
        ("{{ process }}", "undefined value"),
        ("{{ network }}", "undefined value"),
        ("{{ math.random(1, 10) }}", "unknown method"),
        ("{{ date.now }}", "undefined value"),
        ("{{ string.upcase('x') }}", "unknown method"),
    ] {
        let error = native_error(engine(text).render(&Map::new(), &CancellationState::Active));
        assert!(error.contains(category), "{text}: {error}");
    }
}

#[test]
fn m8_owned_json_types_are_data_not_host_callables() {
    let values = arguments(json!({"callable":{"name":"exec"},"items":[null,true,42,"safe",{}]}));
    let error =
        native_error(engine("{{ callable() }}").render(&values, &CancellationState::Active));
    assert!(error.contains("not callable"), "{error}");
    assert_eq!(
        engine("{{ items | length }}")
            .render(&values, &CancellationState::Active)
            .unwrap(),
        "5"
    );
}
