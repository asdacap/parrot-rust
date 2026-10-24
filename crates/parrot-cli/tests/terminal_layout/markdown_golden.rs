//! Independent raw-v2 regressions captured from the pinned .NET source.
use parrot_cli::terminal_layout::{MarkdownRenderer, TerminalText, ToolYamlFormatter};
use serde_json::{Value, json};

macro_rules! source_case {
    ($name:ident, $id:literal $(, $reason:literal)?) => {
        #[test]
        $(#[ignore = $reason])?
        fn $name() {
            let cases: Value = serde_json::from_str(include_str!("fixtures/markdown-cases.json")).unwrap();
            let case = cases.as_array().unwrap().iter()
                .find(|case| case["id"].as_str() == Some($id))
                .expect("registered source fixture exists");
            markdown_and_yaml_match_golden_cases(case);
        }
    };
}

source_case!(
    terminal_layout_emoji_arbitrary_zwj,
    "terminal.layout.emoji-arbitrary-zwj"
);
source_case!(
    markdown_review_heading_inline,
    "markdown.review.heading-inline",
    "Legacy heading, link and ANSI presentation"
);
source_case!(
    markdown_review_table_inline,
    "markdown.review.table-inline",
    "Legacy pipe-less table syntax and inline styles"
);
source_case!(
    markdown_review_table_fallback_inline,
    "markdown.review.table-fallback-inline",
    "Legacy narrow-table key/value fallback"
);
source_case!(
    markdown_review_empty_delimiter_2star,
    "markdown.review.empty-delimiter.2*",
    "Legacy empty bold delimiter handling"
);
source_case!(
    markdown_review_empty_delimiter_2_underscore,
    "markdown.review.empty-delimiter.2_"
);
source_case!(
    markdown_review_empty_delimiter_2tilde,
    "markdown.review.empty-delimiter.2~",
    "Legacy empty strikeout delimiter handling"
);
source_case!(
    markdown_review_empty_delimiter_1star,
    "markdown.review.empty-delimiter.1*"
);
source_case!(
    markdown_review_empty_delimiter_1_underscore,
    "markdown.review.empty-delimiter.1_"
);
source_case!(
    markdown_review_unicode_symbol_escape,
    "markdown.review.unicode-symbol-escape",
    "Legacy Unicode punctuation escaping"
);
source_case!(
    markdown_review_highlighter_state_number,
    "markdown.review.highlighter-state-number",
    "Legacy exact token colors and custom grammar"
);
source_case!(
    markdown_review_trailing_spacing,
    "markdown.review.trailing-spacing"
);
source_case!(
    markdown_review_whitespace_code_line,
    "markdown.review.whitespace-code-line"
);
source_case!(
    yaml_review_date_key_1773cc7308d2,
    "yaml.review.date-key.1773cc7308d2",
    "Rust formatter always double-quotes string mapping keys"
);
source_case!(
    yaml_review_date_key_f1b2d9fc00ea,
    "yaml.review.date-key.f1b2d9fc00ea",
    "Rust formatter always double-quotes string mapping keys"
);
source_case!(
    yaml_review_date_key_402089f6358f,
    "yaml.review.date-key.402089f6358f",
    "Rust formatter always double-quotes string mapping keys"
);
source_case!(
    yaml_review_date_key_51fa106d06f4,
    "yaml.review.date-key.51fa106d06f4"
);
source_case!(
    yaml_review_date_key_8d31f7e4300a,
    "yaml.review.date-key.8d31f7e4300a"
);
source_case!(
    yaml_review_date_key_4c65374b7c60,
    "yaml.review.date-key.4c65374b7c60"
);
source_case!(
    yaml_review_date_key_d26e5bd4b503,
    "yaml.review.date-key.d26e5bd4b503"
);

fn markdown_and_yaml_match_golden_cases(case: &Value) {
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
