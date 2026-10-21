use parrot_cli::terminal_layout::{MarkdownLiveRenderer, MarkdownRenderer, TerminalText};
use serde_json::{Value, json};
use unicode_general_category::GeneralCategory;

#[test]
fn layout_and_markdown_match_golden_cases() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/layout-cases.json")).unwrap();

    for case in cases.as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let api = case["api"].as_str().unwrap();
        let input = case["input"].clone();
        let expected = case["expected"].clone();
        let actual =
            if api.ends_with("TerminalText.LayoutWords") && id != "terminal.layout.hard-break" {
                json!(TerminalText::layout_words(
                    input["value"].as_str().unwrap(),
                    input["width"].as_u64().unwrap() as usize,
                ))
            } else if api.ends_with("TerminalText.LayoutWordsHanging") {
                json!(TerminalText::layout_words_hanging(
                    input["value"].as_str().unwrap(),
                    input["width"].as_u64().unwrap() as usize,
                    input["indent"].as_str().unwrap(),
                ))
            } else if api.ends_with("TerminalText.Layout") {
                json!(TerminalText::layout(
                    input["value"].as_str().unwrap(),
                    input["width"].as_u64().unwrap() as usize,
                ))
            } else if api.ends_with("TerminalText.Sanitize") {
                json!({
                    "sanitized": TerminalText::sanitize("a\tb\u{1b}[2J\u{7}c"),
                    "line": TerminalText::sanitize_line("a\nb"),
                })
            } else if api.contains("Rune.GetUnicodeCategory+TerminalText.Width") {
                let category = unicode_general_category::get_general_category(
                    char::from_u32(
                        u32::from_str_radix(input["codepoint"].as_str().unwrap(), 16).unwrap(),
                    )
                    .unwrap(),
                );
                let category_name = match category {
                    GeneralCategory::NonspacingMark => "NonSpacingMark",
                    GeneralCategory::EnclosingMark => "EnclosingMark",
                    _ => "Other",
                };
                json!({
                    "category": category_name,
                    "cell_width": TerminalText::width(input["text"].as_str().unwrap()),
                })
            } else if id == "terminal.layout.hard-break" {
                json!({
                    "hard": TerminalText::layout_words("abcdefgh", 3),
                    "mixed": TerminalText::layout_words("ab supercalifrag", 5),
                })
            } else if id.starts_with("markdown.blank.") {
                json!({
                    "blank": MarkdownRenderer::render(
                        input["prefix"].as_str().unwrap(),
                        input["source"].as_str().unwrap(),
                        input["columns"].as_u64().unwrap() as usize,
                        input["color"].as_bool().unwrap(),
                    ),
                    "leading": MarkdownRenderer::render(
                        input["prefix"].as_str().unwrap(),
                        "\ndef",
                        input["columns"].as_u64().unwrap() as usize,
                        input["color"].as_bool().unwrap(),
                    ),
                })
            } else if id == "markdown.ansi-no-color" {
                json!({
                    "colored": MarkdownRenderer::render(
                        input["prefix"].as_str().unwrap(),
                        input["source"].as_str().unwrap(),
                        input["columns"].as_u64().unwrap() as usize,
                        true,
                    ),
                    "plain": MarkdownRenderer::render(
                        input["prefix"].as_str().unwrap(),
                        input["source"].as_str().unwrap(),
                        input["columns"].as_u64().unwrap() as usize,
                        false,
                    ),
                })
            } else if api.ends_with("MarkdownRenderer.Render") {
                json!(MarkdownRenderer::render(
                    input["prefix"].as_str().unwrap(),
                    input["source"].as_str().unwrap(),
                    input["columns"].as_u64().unwrap() as usize,
                    input["color"].as_bool().unwrap(),
                ))
            } else if api.contains("MarkdownLiveRenderer.Append+Commit") {
                render_live_commit(id, &input)
            } else if api.contains("MarkdownLiveRenderer.Append") {
                render_live_append(id, &input)
            } else {
                panic!("unmapped source API for {id}: {api}");
            };
        assert_eq!(actual, expected, "source oracle mismatch for {id}");
    }
}

fn source_display_string(value: &str) -> String {
    value
        .replace("\\u0060", "`")
        .replace("\\u25CF", "●")
        .replace("\\n", "\n")
}

fn render_live_commit(id: &str, input: &Value) -> Value {
    if id == "markdown.live.promote-commit" {
        let mut renderer = MarkdownLiveRenderer::new(input["width"].as_u64().unwrap() as usize);
        let prefix = source_display_string(input["prefix"].as_str().unwrap());
        let first_text = source_display_string(input["first"].as_str().unwrap());
        let second_text = source_display_string(input["second"].as_str().unwrap());
        let first = renderer
            .append("answer", &prefix, &first_text, false)
            .unwrap();
        let promoted = renderer
            .append("answer", &prefix, &second_text, false)
            .unwrap();
        let committed = renderer.commit(false).unwrap();
        json!({
            "first_scrollback": first.scrollback,
            "first_preview": first.preview,
            "promoted_scrollback": promoted.scrollback,
            "promotion_preview": promoted.preview,
            "committed": committed.scrollback,
        })
    } else {
        let mut renderer = MarkdownLiveRenderer::new(80);
        let prefix = source_display_string(input["prefix"].as_str().unwrap());
        let first_text = source_display_string(input["first"].as_str().unwrap());
        let second_text = source_display_string(input["second"].as_str().unwrap());
        let first = renderer
            .append("answer", &prefix, &first_text, false)
            .unwrap();
        let second = renderer
            .append("answer", &prefix, &second_text, false)
            .unwrap();
        let committed = renderer.commit(false).unwrap();
        json!({
            "first_scrollback": first.scrollback,
            "first_preview": first.preview,
            "second_scrollback": second.scrollback,
            "second_preview": second.preview,
            "committed": committed.scrollback,
        })
    }
}

fn render_live_append(id: &str, input: &Value) -> Value {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let opening = source_display_string(input["opening"].as_str().unwrap());
    let first = renderer.append("answer", "", &opening, false).unwrap();
    if id == "markdown.live.fence" {
        let closing = source_display_string(input["closing"].as_str().unwrap());
        let second = renderer.append("answer", "", &closing, false).unwrap();
        json!({
            "open_scrollback": first.scrollback,
            "open_preview": first.preview,
            "close_scrollback": second.scrollback,
            "close_preview": second.preview,
        })
    } else {
        let mut table = MarkdownLiveRenderer::new(80);
        let open = table.append("table", "", &opening, false).unwrap();
        let boundary_text = source_display_string(input["boundary"].as_str().unwrap());
        let boundary = table.append("table", "", &boundary_text, false).unwrap();
        json!({
            "open_scrollback": open.scrollback,
            "open_preview": open.preview,
            "boundary_scrollback": boundary.scrollback,
            "boundary_preview": boundary.preview,
        })
    }
}
