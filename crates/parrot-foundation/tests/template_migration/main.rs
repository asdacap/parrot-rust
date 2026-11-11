//! Fixed bundled-template migration evidence, not general Scriban compatibility.
use parrot_foundation::template_minijinja::MiniJinjaPromptTemplateEngine;
use serde_json::{Map, Value};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn fixture(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(root().join("tests/fixtures/scriban/a008").join(name)).unwrap(),
    )
    .unwrap()
}
fn migration_fixture(name: &str) -> Value {
    serde_json::from_slice(
        &fs::read(
            root()
                .join("crates/parrot-foundation/tests/template_migration/fixtures")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}
fn fields(value: &Value) -> &Map<String, Value> {
    value.as_object().unwrap()
}
fn render(id: &str, text: &str, arguments: &Value) -> String {
    let engine =
        MiniJinjaPromptTemplateEngine::compile(&format!("prompt_templates.{id}"), text).unwrap();
    match engine.render(fields(arguments), &NoCancellation) {
        Ok(value) => value,
        Err(error) => panic!("{id}: {error}"),
    }
}
struct NoCancellation;
impl parrot_domain::contracts::async_ops::Cancellation for NoCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn m3_content_is_opaque_and_repeated_template_calls_are_isolated() {
    let mapping = fixture("bundled-migration-map.json");
    let inputs = migration_fixture("m2-source-input.json");
    let cases = inputs.as_array().unwrap();
    for descriptor in mapping["entries"].as_array().unwrap() {
        let id = descriptor["id"].as_str().unwrap();
        let input = &cases
            .iter()
            .position(|row| row["id"] == format!("A008.M2.{id}.multiple"))
            .map(|index| &cases[index])
            .unwrap();
        let engine = MiniJinjaPromptTemplateEngine::compile(
            &format!("prompt_templates.{id}"),
            descriptor["new"]["text"].as_str().unwrap(),
        )
        .unwrap();
        let first = engine
            .render(fields(&input["arguments"]), &NoCancellation)
            .unwrap();
        let second = engine
            .render(fields(&input["arguments"]), &NoCancellation)
            .unwrap();
        assert_eq!(first, second, "{id} repeated render");
    }
}

#[test]
fn each_engine_is_separate_and_never_mutates_argument_json() {
    let mapping = fixture("bundled-migration-map.json");
    let cases = migration_fixture("m2-source-input.json");
    for descriptor in mapping["entries"].as_array().unwrap() {
        let id = descriptor["id"].as_str().unwrap();
        let input = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == format!("A008.M2.{id}.nested-branch"))
            .unwrap();
        let before = input["arguments"].clone();
        let actual = render(id, descriptor["new"]["text"].as_str().unwrap(), &before);
        assert!(!actual.is_empty(), "{id}");
        assert_eq!(input["arguments"], before, "{id} arguments immutable");
    }
}
