mod termimad_adapter;
mod yaml_cases;
mod date_keys;
mod final_format;
mod markdown_regressions;
mod markdown_round3;
mod markdown_round4_wrap;
mod golden;
mod markdown_golden;
mod round3_final_format;

use parrot_cli::terminal_layout::{
    MarkdownLiveRenderer, MarkdownRenderer, TerminalCellStyleSpan, TerminalLine, TerminalStyle,
    TerminalText,
};

#[test]
fn unicode16_spacing_mark_observations_have_zero_cell_width() {
    for codepoint in [0x113ce, 0x113d0, 0x1612f, 0x1e5ee, 0x1e5ef] {
        let mark = char::from_u32(codepoint).unwrap();
        assert_eq!(
            unicode_general_category::get_general_category(mark),
            unicode_general_category::GeneralCategory::NonspacingMark
        );
        assert_eq!(TerminalText::width(&format!("{mark}x")), 1);
    }
}

#[test]
fn terminal_line_owns_spans_and_compares_all_fields() {
    let spans = vec![TerminalCellStyleSpan::new(
        1,
        2,
        TerminalStyle::new("\x1b[31m"),
    )];
    let line = TerminalLine::with_spans("text", TerminalStyle::default(), spans.clone());
    let same = TerminalLine::with_spans("text", TerminalStyle::default(), spans);
    assert_eq!(line, same);
    assert_eq!(line.text(), "text");
    assert_eq!(line.style_spans().len(), 1);
    assert_ne!(line, TerminalLine::new("text", TerminalStyle::new("base")));
    assert_eq!(
        TerminalStyle::new("\x1b[31m").apply("red"),
        "\x1b[31mred\x1b[0m"
    );
    assert_eq!(TerminalStyle::default().apply("plain"), "plain");
}

#[test]
fn word_layout_wraps_at_word_boundaries_and_keeps_newlines() {
    let cases = [
        ("hello world", 7, "hello|world"),
        ("alpha beta gamma", 11, "alpha beta|gamma"),
        ("  lead", 10, "  lead"),
        ("one\ntwo", 80, "one|two"),
        ("ab\ncd", 3, "ab|cd"),
        ("ab cd", 5, "ab cd"),
        ("ab cde", 5, "ab|cde"),
    ];
    for (value, width, expected) in cases {
        let rows = TerminalText::layout_words(value, width);
        assert_eq!(rows.join("|"), expected);
        assert!(rows.iter().all(|row| TerminalText::width(row) <= width));
    }
}

#[test]
fn word_layout_keeps_wide_and_emoji_graphemes_intact() {
    for (value, width, expected) in [
        ("日本語 alpha", 7, "日本語|alpha"),
        ("界界界", 5, "界界|界"),
        ("a👨‍👩‍👧‍👦b", 3, "a👨‍👩‍👧‍👦|b"),
    ] {
        let rows = TerminalText::layout_words(value, width);
        assert_eq!(rows.join("|"), expected);
        assert!(rows.iter().all(|row| TerminalText::width(row) <= width));
    }
}

#[test]
fn word_layout_hard_breaks_long_tokens() {
    assert_eq!(
        TerminalText::layout_words("abcdefgh", 3).join("|"),
        "abc|def|gh"
    );
    let rows = TerminalText::layout_words("ab supercalifrag", 5);
    assert_eq!(rows.join("|"), "ab su|perca|lifra|g");
    assert!(rows.iter().all(|row| TerminalText::width(row) <= 5));
}

#[test]
fn word_layout_hangs_continuations_at_indent() {
    assert_eq!(
        TerminalText::layout_words_hanging("alpha beta gamma", 11, "  ").join("|"),
        "alpha beta|  gamma"
    );
    assert_eq!(
        TerminalText::layout_words_hanging("ab supercalifrag", 5, "  ").join("|"),
        "ab su|  per|  cal|  ifr|  ag"
    );
}

#[test]
fn cell_layout_wraps_and_preserves_hanging_indentation() {
    assert_eq!(TerminalText::layout("abcdef", 3).join("|"), "abc|def");
    assert_eq!(
        TerminalText::layout_hanging("a\nbcdef", 3, "  ").join("|"),
        "a|  b|  c|  d|  e|  f"
    );
}

#[test]
fn sanitization_replaces_untrusted_controls_and_expands_tabs() {
    assert_eq!(TerminalText::sanitize("a\tb\u{1b}[2J\u{7}c"), "a    b[2Jc");
    assert_eq!(TerminalText::sanitize_line("a\nb"), "a b");
}

#[test]
fn markdown_formats_block_and_inline_elements_without_ansi_in_no_color_mode() {
    let source = "# Heading\n> quoted\n- item\n- [x] done\n1. ordered\n**bold** *italic* ~~gone~~ `code` [site](https://example.com)\n---";
    let rendered = MarkdownRenderer::render("- ", source, 80, false);
    assert_eq!(
        rendered.join("\n"),
        format!(
            "- Heading\n  │ quoted\n  • item\n  • ☑ done\n  1. ordered\n  bold italic gone code site (https://example.com)\n  {}",
            "─".repeat(78)
        )
    );
    assert!(!rendered.iter().any(|line| line.contains('\u{1b}')));
}

#[test]
fn markdown_supports_muted_hanging_prefix_content_layout() {
    assert_eq!(
        MarkdownRenderer::render("✦ ", "# title\n- **item**", 80, false),
        ["✦ title", "  • item"]
    );
    assert_eq!(
        MarkdownRenderer::render("✦ ", "abcdefg", 8, false),
        ["✦ abcdef", "  g"]
    );
}

#[test]
fn markdown_drops_blank_edges_and_preserves_blank_content_lines() {
    assert!(MarkdownRenderer::render("● ", "\n", 80, false).is_empty());
    assert_eq!(
        MarkdownRenderer::render("● ", "\ndef", 80, false),
        ["● def"]
    );
    assert_eq!(
        MarkdownRenderer::render("", "one\n\ntwo", 80, false),
        ["one", "", "two"]
    );
}

#[test]
fn markdown_formats_tables_and_fenced_code() {
    let source = "| Name | Status |\n| --- | --- |\n| Build | Done |\n\n```csharp\npublic var value = 42;\n```";
    let rendered = MarkdownRenderer::render("", source, 80, false).join("\n");
    assert!(rendered.contains("┌───────┬────────┐"), "{rendered}");
    assert!(rendered.contains("│ Name  │ Status │"), "{rendered}");
    assert!(rendered.contains("public var value = 42;"), "{rendered}");
    assert!(!rendered.contains("```"));
}

#[test]
fn markdown_wraps_at_source_cell_width_and_keeps_wide_graphemes() {
    let rows = MarkdownRenderer::render("", "日本語 alpha", 7, false);
    assert_eq!(rows, ["日本語", "alpha"]);
    assert!(rows.iter().all(|line| TerminalText::width(line) <= 7));
}

#[test]
fn markdown_emits_known_language_ansi_but_no_color_is_text_identical() {
    let plain = MarkdownRenderer::render(
        "- ",
        "# Heading\n```csharp\npublic string Value() => \"ok\"; // note\n```",
        80,
        false,
    )
    .join("\n");
    let colored = MarkdownRenderer::render(
        "- ",
        "# Heading\n```csharp\npublic string Value() => \"ok\"; // note\n```",
        80,
        true,
    )
    .join("\n");
    let unknown =
        MarkdownRenderer::render("", "```not-a-language\npublic value = 1\n```", 80, true)
            .join("\n");
    assert!(colored.contains("\x1b[1;36mHeading\x1b[0m"));
    assert!(colored.contains("\x1b[1;35mpublic\x1b[0m"));
    assert!(colored.contains("\x1b[32m\"ok\"\x1b[0m"));
    assert!(colored.contains("\x1b[2;3;90m// note\x1b[0m"));
    assert!(!unknown.contains("\x1b[1;35m"));
    assert!(!plain.contains('\u{1b}'));
    assert_eq!(strip_ansi(&colored), plain);
}

#[test]
fn markdown_does_not_emit_unsafe_terminal_control_sequences() {
    let rendered = MarkdownRenderer::render("", "**safe\x1b[2J**", 80, true).join("\n");
    assert!(!rendered.contains("\x1b[2J"));
    assert_eq!(strip_ansi(&rendered), "safe[2J");
}

#[test]
fn markdown_tables_fall_back_to_key_value_lines_when_too_wide() {
    let rows = MarkdownRenderer::render(
        "",
        "| Name | Status |\n| --- | --- |\n| Build | Done |",
        15,
        false,
    );
    assert!(
        rows.iter().any(|line| line.contains("Name: Build")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|line| line.contains("Status: Done")),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|line| line.contains('┌')));
}

#[test]
fn markdown_live_promotes_complete_lines_and_keeps_committed_output_stable() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let first = renderer.append("answer", "- ", "# Head", false).unwrap();
    let update = renderer
        .append("answer", "- ", "ing\n**bold**", false)
        .unwrap();
    let committed = renderer.commit(false).unwrap();
    assert!(first.scrollback.is_none());
    assert!(
        update
            .scrollback
            .unwrap()
            .iter()
            .any(|line| line == "- Heading")
    );
    assert_eq!(committed.scrollback.unwrap(), ["  bold"]);
}

#[test]
fn markdown_live_keeps_promoted_json_as_markdown_but_formats_unpromoted_json_at_commit() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let promoted = renderer
        .append(
            "answer",
            "assistant: ",
            "Explanation:\n{\"value\":1}",
            false,
        )
        .unwrap();
    assert!(
        promoted
            .scrollback
            .unwrap()
            .iter()
            .any(|line| line.contains("Explanation:"))
    );
    let committed = renderer.commit(false).unwrap().scrollback.unwrap();
    assert!(committed.iter().any(|line| line.contains(r#"{"value":1}"#)));
    assert!(!committed.iter().any(|line| line.contains("value: 1")));

    let mut markdown = MarkdownLiveRenderer::new(80);
    markdown
        .append("plain", "assistant: ", "**bold**", false)
        .unwrap();
    let committed = markdown.commit(false).unwrap().scrollback.unwrap();
    assert!(committed.iter().any(|line| line.contains("bold")));
    assert!(!committed.iter().any(|line| line.contains("```yaml")));
}

#[test]
fn markdown_live_keeps_one_unwrapped_source_until_commit() {
    let mut renderer = MarkdownLiveRenderer::new(8);
    let source = "```text\none\ntwo\nthree\nfour\nfive";
    let update = renderer.append("answer", "", source, false).unwrap();
    assert!(update.scrollback.is_none());
    assert_eq!(update.preview.join("|"), source);
    let committed = renderer
        .commit(false)
        .unwrap()
        .scrollback
        .unwrap()
        .join("\n");
    assert!(committed.contains("one\ntwo\nthree\nfour\nfive"));
}

#[test]
fn markdown_live_keeps_fences_and_tables_uncommitted_until_their_boundary() {
    let mut fenced = MarkdownLiveRenderer::new(80);
    let open = fenced
        .append("answer", "", "```rust\nfn main() {}\n", false)
        .unwrap();
    assert!(open.scrollback.is_none());
    assert_eq!(open.preview.join(""), "```rust\nfn main() {}\n");
    let close = fenced.append("answer", "", "```\n", false).unwrap();
    assert!(
        close
            .scrollback
            .unwrap()
            .iter()
            .any(|line| line.contains("fn main"))
    );

    let mut table = MarkdownLiveRenderer::new(80);
    let open = table
        .append("table", "", "A | B\n--- | ---\nx | y\n", false)
        .unwrap();
    assert!(open.scrollback.is_none());
    let boundary = table.append("table", "", "after\n", false).unwrap();
    assert!(
        boundary
            .scrollback
            .unwrap()
            .iter()
            .any(|line| line.contains("┌───┬───┐"))
    );
}

#[test]
fn markdown_live_rejects_empty_or_changed_stream_identity_and_prefix() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    assert!(renderer.append("", "", "text", false).is_err());
    renderer.append("one", "p ", "pending", false).unwrap();
    assert!(renderer.append("two", "p ", "x", false).is_err());
    assert!(renderer.append("one", "q ", "x", false).is_err());
}

fn strip_ansi(value: &str) -> String {
    let mut out = String::new();
    let mut it = value.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
