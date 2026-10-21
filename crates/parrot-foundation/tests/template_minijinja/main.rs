use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::config::ConfigError;
use parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const PATH: &str = "prompt_templates.custom.adapter";
// Each native range stays below its separate size guard; nested work exhausts fuel.
const NO_OUTPUT_FUEL: &str =
    "{% for i in range(1001) %}{% for j in range(1001) %}{% set x = j %}{% endfor %}{% endfor %}";

/// A deterministic token that cancels at an exact adapter observation boundary.
/// No thread, sleep, native hook or wall-clock assumption is involved.
struct CancelAt {
    calls: AtomicUsize,
    at: usize,
}

impl CancelAt {
    fn new(at: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            at,
        }
    }

    fn never() -> Self {
        Self::new(usize::MAX)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Cancellation for CancelAt {
    fn is_cancelled(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst) + 1 >= self.at
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
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
            compiled.render(values, &CancelAt::never()).unwrap(),
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
            compiled.render(&first, &CancelAt::never()).unwrap(),
            "first"
        );
        assert_eq!(
            compiled.render(&second, &CancelAt::never()).unwrap(),
            "second"
        );
    }
    assert_eq!(first, saved);
    let other = engine("{{ value }}");
    native_error(other.render(&Map::new(), &CancelAt::never()));
    assert_eq!(other.render(&first, &CancelAt::never()).unwrap(), "first");
}

#[test]
fn m4_missing_variable_is_strict_and_discards_prior_output() {
    let error = native_error(
        engine("PRIVATE-PARTIAL{{ missing }}").render(&Map::new(), &CancelAt::never()),
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
        engine("{{ item.missing }}").render(&arguments(json!({"item":{}})), &CancelAt::never()),
    );
    assert!(error.contains("undefined"), "{error}");
}

#[test]
fn m4_missing_index_is_strict() {
    let error = native_error(
        engine("{{ items[1] }}").render(&arguments(json!({"items":[{}]})), &CancelAt::never()),
    );
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
            compiled.render(values, &CancelAt::never()).unwrap(),
            case["old_expected"]["value"].as_str().unwrap()
        );
    }
}

#[test]
fn m7_precancelled_stops_before_conversion_or_native_error() {
    let token = CancelAt::new(1);
    assert_eq!(
        engine("{{ missing }}").render(&Map::new(), &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 1);
}

#[test]
fn m7_second_checkpoint_stops_before_engine_work() {
    let token = CancelAt::new(2);
    assert_eq!(
        engine("{{ missing }}").render(&Map::new(), &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 2);
}

#[test]
fn m7_output_cancellation_discards_written_prefix_and_is_not_retried() {
    let token = CancelAt::new(4);
    let result =
        engine("prefix{{ value }}suffix").render(&arguments(json!({"value":"data"})), &token);
    assert_eq!(result, Err(OperationError::Cancelled));
    assert_eq!(token.calls(), 5); // two entry checks, two writes, post-engine check
}

#[test]
fn m7_no_output_fuel_failure_is_path_prefixed_and_resets_per_call() {
    let compiled = engine(NO_OUTPUT_FUEL);
    for _ in 0..2 {
        let error = native_error(compiled.render(&Map::new(), &CancelAt::never()));
        assert!(error.contains("engine ran out of fuel"), "{error}");
    }
}

#[test]
fn m7_cancel_after_no_output_fuel_error_wins_without_a_worker_or_vm_hook() {
    let token = CancelAt::new(3);
    assert_eq!(
        engine(NO_OUTPUT_FUEL).render(&Map::new(), &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 3); // no polling inside the no-output native loop
}

#[test]
fn m7_cancel_after_no_output_completion_discards_success() {
    let token = CancelAt::new(3);
    let compiled = engine("{% for i in range(1001) %}{% set x = i %}{% endfor %}");
    assert_eq!(
        compiled.render(&Map::new(), &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 3);
    assert_eq!(
        compiled.render(&Map::new(), &CancelAt::never()).unwrap(),
        ""
    );
}

#[test]
fn m7_cancel_after_native_error_wins_and_discards_error() {
    let token = CancelAt::new(3);
    assert_eq!(
        engine("{{ missing }}").render(&Map::new(), &token),
        Err(OperationError::Cancelled)
    );
    assert_eq!(token.calls(), 3);
}

#[test]
fn m7_fuel_failure_after_output_exposes_no_partial_result() {
    let compiled = engine(
        "prefix{% for i in range(1001) %}{% for j in range(1001) %}x{% endfor %}{% endfor %}",
    );
    let error = native_error(compiled.render(&Map::new(), &CancelAt::never()));
    assert!(error.contains("engine ran out of fuel"), "{error}");
}

#[test]
fn m7_native_recursion_failure_is_bounded_and_no_partial_escapes() {
    let compiled = engine("prefix{% macro recur() %}{{ recur() }}{% endmacro %}{{ recur() }}");
    let error = native_error(compiled.render(&Map::new(), &CancelAt::never()));
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
    let error = native_error(compiled.render(&Map::new(), &CancelAt::never()));
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
        let error = native_error(engine(text).render(&Map::new(), &CancelAt::never()));
        assert!(error.contains(category), "{text}: {error}");
    }
}

#[test]
fn m8_owned_json_types_are_data_not_host_callables() {
    let values = arguments(json!({"callable":{"name":"exec"},"items":[null,true,42,"safe",{}]}));
    let error = native_error(engine("{{ callable() }}").render(&values, &CancelAt::never()));
    assert!(error.contains("not callable"), "{error}");
    assert_eq!(
        engine("{{ items | length }}")
            .render(&values, &CancelAt::never())
            .unwrap(),
        "5"
    );
}
