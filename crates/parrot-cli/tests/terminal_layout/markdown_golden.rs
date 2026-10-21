//! Independent raw-v2 regressions captured from the pinned .NET source.
use parrot_cli::terminal_layout::{MarkdownRenderer, TerminalText, ToolYamlFormatter};
use serde_json::{Value, json};

#[test]
fn markdown_and_yaml_match_golden_cases() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/markdown-cases.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let api = case["api"].as_str().unwrap();
        let input = case["input"].clone();
        let expected = case["expected"].clone();
        let actual = if api.contains("TerminalText.Width+LayoutWords") {
            let value = input["value"].as_str().unwrap();
            let width = input["width"].as_u64().unwrap() as usize;
            json!({
                "width": TerminalText::width(value),
                "rows": TerminalText::layout_words(value, width),
            })
        } else if api.ends_with("MarkdownRenderer.Render") {
            json!(MarkdownRenderer::render(
                input["prefix"].as_str().unwrap(),
                input["source"].as_str().unwrap(),
                input["columns"].as_u64().unwrap() as usize,
                input["color"].as_bool().unwrap(),
            ))
        } else if api.ends_with("ToolYamlFormatter.Format") {
            json!(ToolYamlFormatter::format(input["json"].as_str().unwrap()).unwrap())
        } else {
            panic!("unmapped source API for {id}: {api}");
        };
        assert_eq!(actual, expected, "independent source mismatch for {id}");
    }
}
