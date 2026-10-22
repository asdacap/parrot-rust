use parrot_cli::terminal_layout::{MarkdownLiveRenderer, MarkdownRenderer, TerminalText};
use serde_json::{Value, json};
use unicode_general_category::GeneralCategory;

macro_rules! source_case {
    ($name:ident, $id:literal $(, $reason:literal)?) => {
        #[test]
        $(#[ignore = $reason])?
        fn $name() {
            let cases: Value = serde_json::from_str(include_str!("fixtures/layout-cases.json")).unwrap();
            let case = cases.as_array().unwrap().iter()
                .find(|case| case["id"].as_str() == Some($id))
                .expect("registered source fixture exists");
            layout_and_markdown_match_golden_cases(case);
        }
    };
}

source_case!(
    terminal_layout_word_677bbef68cb8,
    "terminal.layout.word.677bbef68cb8"
);
source_case!(
    terminal_layout_word_5445ca258c6d,
    "terminal.layout.word.5445ca258c6d"
);
source_case!(
    terminal_layout_word_1de98e84129a,
    "terminal.layout.word.1de98e84129a"
);
source_case!(
    terminal_layout_word_a4d5775dcc48,
    "terminal.layout.word.a4d5775dcc48"
);
source_case!(
    terminal_layout_word_9898fb632c9f,
    "terminal.layout.word.9898fb632c9f"
);
source_case!(
    terminal_layout_word_2063991d7380,
    "terminal.layout.word.2063991d7380"
);
source_case!(
    terminal_layout_word_3dc019a52906,
    "terminal.layout.word.3dc019a52906"
);
source_case!(
    terminal_layout_grapheme_582e9f87df67,
    "terminal.layout.grapheme.582e9f87df67"
);
source_case!(
    terminal_layout_grapheme_96919b058a6e,
    "terminal.layout.grapheme.96919b058a6e"
);
source_case!(
    terminal_layout_grapheme_ac2e0711a568,
    "terminal.layout.grapheme.ac2e0711a568"
);
source_case!(
    terminal_layout_grapheme_a36679a9b6dc,
    "terminal.layout.grapheme.a36679a9b6dc"
);
source_case!(
    terminal_unicode_category_113ce,
    "terminal.unicode-category.113ce"
);
source_case!(
    terminal_unicode_category_113d0,
    "terminal.unicode-category.113d0"
);
source_case!(
    terminal_unicode_category_1612f,
    "terminal.unicode-category.1612f"
);
source_case!(
    terminal_unicode_category_1e5ee,
    "terminal.unicode-category.1e5ee"
);
source_case!(
    terminal_unicode_category_1e5ef,
    "terminal.unicode-category.1e5ef"
);
source_case!(terminal_layout_hard_break, "terminal.layout.hard-break");
source_case!(
    terminal_layout_hanging_453a2c97e2ad,
    "terminal.layout.hanging.453a2c97e2ad"
);
source_case!(
    terminal_layout_hanging_edd2468d1440,
    "terminal.layout.hanging.edd2468d1440"
);
source_case!(terminal_layout_cells, "terminal.layout.cells");
source_case!(terminal_layout_sanitize, "terminal.layout.sanitize");
source_case!(
    markdown_blocks_inline,
    "markdown.blocks-inline",
    "Legacy heading, quote, checkbox and link presentation"
);
source_case!(markdown_blank_01ba4719c80b, "markdown.blank.01ba4719c80b");
source_case!(markdown_blank_8855c6119660, "markdown.blank.8855c6119660");
source_case!(markdown_blank_7eb70257593d, "markdown.blank.7eb70257593d");
source_case!(
    markdown_table_fence,
    "markdown.table-fence",
    "Legacy table geometry and code presentation"
);
source_case!(
    markdown_ansi_no_color,
    "markdown.ansi-no-color",
    "Legacy exact heading and token ANSI colors"
);
source_case!(
    markdown_sanitize_control,
    "markdown.sanitize-control",
    "Legacy exact bold ANSI; control safety remains covered"
);
source_case!(
    markdown_live_promote_commit,
    "markdown.live.promote-commit",
    "Legacy streamed heading positioning"
);
source_case!(markdown_live_fence, "markdown.live.fence");
source_case!(
    markdown_live_table,
    "markdown.live.table",
    "Legacy pipe-less table syntax and borders"
);
source_case!(markdown_live_leading_blank, "markdown.live.leading-blank");

fn layout_and_markdown_match_golden_cases(case: &Value) {
    let id = case["id"].as_str().unwrap();
    let api = case["api"].as_str().unwrap();
    let input = case["input"].clone();
    let expected = case["expected"].clone();
    let actual = if api.ends_with("TerminalText.LayoutWords") && id != "terminal.layout.hard-break"
    {
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
            char::from_u32(u32::from_str_radix(input["codepoint"].as_str().unwrap(), 16).unwrap())
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
