//! Focused parity cases for the pinned MarkdownRenderer.cs and TerminalText.cs
//! at 5f27a8a92625ee31d8bec094e5c3e09550193781. Expectations follow source
//! StringInfo, Inline/Style.Merge, CodeRuns/AddCode, and AddRuns behavior.
use parrot_cli::terminal_layout::{MarkdownRenderer, TerminalText};

fn ansi(code: &str, text: &str) -> String {
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn render(source: &str, color: bool) -> Vec<String> {
    MarkdownRenderer::render("", source, 120, color)
}

fn code(language: &str, source: &str) -> Vec<String> {
    render(&format!("~~~{language}\n{source}\n~~~"), true)
}

#[test]
fn extended_graphemes_do_not_join_arbitrary_letters_after_zwj() {
    let text = "a\u{200d}b";
    assert_eq!(TerminalText::width(text), 2);
    assert_eq!(TerminalText::clip(text, 1), "a\u{200d}");
    assert_eq!(TerminalText::layout(text, 1), ["a\u{200d}", "b"]);
    assert_eq!(TerminalText::layout_words(text, 1), ["a\u{200d}", "b"]);
    assert_eq!(
        MarkdownRenderer::render("", text, 1, false),
        ["a\u{200d}", "b"]
    );
    // Hangul and spacing marks also require extended, not scalar grouping.
    assert_eq!(TerminalText::clip("\u{1100}\u{1161}x", 2), "");
    assert_eq!(TerminalText::clip("क\u{093e}x", 1), "");
    assert_eq!(TerminalText::clip("👩\u{200d}💻x", 2), "👩\u{200d}💻");
}

#[test]
#[ignore = "Legacy heading, link and ANSI presentation"]
fn headings_parse_inline_and_merge_the_heading_basis() {
    let source = "# **bold** *em* ~~gone~~ `code` [site](url)";
    assert_eq!(render(source, false), ["bold em gone code site (url)"]);
    let colored = render(source, true);
    assert_eq!(
        crate::strip_ansi(&colored.join("\n")),
        render(source, false).join("\n")
    );
    assert_eq!(
        colored,
        [ansi("1;36", "bold ")
            + &ansi("1;3;36", "em")
            + &ansi("1;36", " ")
            + &ansi("1;9;36", "gone")
            + &ansi("1;36", " ")
            + &ansi("1;33", "code")
            + &ansi("1;36", " ")
            + &ansi("1;4;36", "site (url)")]
    );
}

#[test]
#[ignore = "Legacy pipe-less table syntax and widths"]
fn table_cells_use_inline_styles_but_source_widths_remain_raw() {
    let source = "**A** | B\n--- | ---\n**bold** | `x`";
    assert_eq!(
        render(source, false),
        [
            "┌──────────┬─────┐",
            "│ A    │ B   │",
            "├──────────┼─────┤",
            "│ bold │ x │",
            "└──────────┴─────┘",
        ]
    );
    let actual = render(source, true);
    assert_eq!(
        crate::strip_ansi(&actual.join("\n")),
        render(source, false).join("\n")
    );
    assert_eq!(actual[0], ansi("2;90", "┌──────────┬─────┐"));
    assert_eq!(
        actual[1],
        ansi("2;90", "│")
            + &ansi("1;36", " A")
            + &ansi("38;5;195", "    ")
            + &ansi("2;90", "│")
            + &ansi("1;36", " B")
            + &ansi("38;5;195", "   ")
            + &ansi("2;90", "│")
    );
    assert_eq!(
        actual[3],
        ansi("2;90", "│")
            + &ansi("38;5;195", " ")
            + &ansi("1;38;5;195", "bold")
            + &ansi("38;5;195", " ")
            + &ansi("2;90", "│")
            + &ansi("38;5;195", " ")
            + &ansi("33", "x")
            + &ansi("38;5;195", " ")
            + &ansi("2;90", "│")
    );
}

#[test]
#[ignore = "Legacy key/value table fallback and ANSI palette"]
fn table_fallback_headers_parse_inline_using_bold_cyan_basis() {
    let source = "*A* | B\n--- | ---\n**v** | x";
    assert_eq!(
        MarkdownRenderer::render("", source, 12, false),
        ["A: v", "B: x"]
    );
    let colored = MarkdownRenderer::render("", source, 12, true);
    assert_eq!(
        crate::strip_ansi(&colored.join("\n")),
        MarkdownRenderer::render("", source, 12, false).join("\n")
    );
    assert_eq!(
        colored,
        [
            ansi("1;3;36", "A") + &ansi("1;36", ": ") + &ansi("1;38;5;195", "v"),
            ansi("1;36", "B: ") + &ansi("38;5;195", "x"),
        ]
    );
}

#[test]
#[ignore = "Legacy delimiter and Unicode escaping rules"]
fn empty_delimiters_stay_literal_and_unicode_bmp_symbols_escape() {
    for value in ["**", "__", "~~", "a **** b", "a ____ b", "a ** b", "a ~~ b"] {
        assert_eq!(render(value, false), [value], "{value:?}");
    }
    assert_eq!(render(r"\€ \。 \© \× \a \9", false), [r"€ 。 © × \a \9"]);
    // char.IsSymbol receives a UTF-16 char, so a supplementary symbol is not escaped.
    assert_eq!(render("\\😀", false), ["\\😀"]);
    assert_eq!(render(r"# \€ **", true), [ansi("1;36", "€ **")]);
}

#[test]
#[ignore = "Legacy tilde fences and exact custom highlighter output"]
fn code_comments_and_triple_strings_keep_cross_line_state() {
    assert_eq!(
        code(
            "rust",
            "/* public\n42 */ let\n\"\"\"start\n# 42\nend\"\"\" return"
        ),
        [
            ansi("2;3;90", "/* public"),
            ansi("2;3;90", "42 */") + &ansi("38;5;195", " ") + &ansi("1;35", "let"),
            ansi("32", "\"\"\"start"),
            ansi("32", "# 42"),
            ansi("32", "end\"\"\"") + &ansi("38;5;195", " ") + &ansi("1;35", "return"),
        ]
    );
    // State belongs to one code fence, not to the renderer or its next block.
    assert_eq!(
        render("~~~rust\n/*\n~~~\n~~~rust\nreturn\n~~~", true),
        [ansi("2;3;90", "/*"), ansi("1;35", "return"),]
    );
}

#[test]
#[ignore = "Legacy tilde fences and exact token ANSI"]
fn code_backticks_hash_comments_numbers_and_all_aliases_are_highlighted() {
    let expected = ansi("32", "`hi`")
        + &ansi("38;5;195", " ")
        + &ansi("33", "42.0_f")
        + &ansi("38;5;195", " ")
        + &ansi("2;3;90", "# public");
    for language in [
        "rust", "c-sharp", "c#", "golang", "python3", "py3", "shell", "sh", "zsh",
    ] {
        assert_eq!(
            code(language, "`hi` 42.0_f # public"),
            [expected.clone()],
            "{language}"
        );
    }
    assert_eq!(
        code("unknown", "42 # public"),
        [ansi("38;5;195", "42 # public")]
    );
    assert_eq!(
        code("rust", "call (42) // public"),
        [ansi("36", "call")
            + &ansi("38;5;195", " (")
            + &ansi("33", "42")
            + &ansi("38;5;195", ") ")
            + &ansi("2;3;90", "// public")]
    );
}

#[test]
#[ignore = "Legacy language-independent keyword recognizer"]
fn code_recognizes_the_complete_source_keyword_set() {
    let keywords = "abstract and as async await base bool break case catch char class const continue def defer do else enum except export extends false finally float for foreach from func function go if implements import in int interface internal is let map match namespace new nil not null object of or out override package pass private protected public readonly record return sealed static string struct super switch this throw throws true try type typeof undefined using var virtual void when where while with yield";
    for keyword in keywords.split(' ') {
        assert_eq!(code("rust", keyword), [ansi("1;35", keyword)], "{keyword}");
    }
}

#[test]
#[ignore = "Legacy tilde fences; inclusive library highlighting limits have active adapter tests"]
fn code_highlighting_caps_are_inclusive_and_count_utf16_units_plus_newlines() {
    let at_lines = "42\n".repeat(9_999) + "42";
    let at = code("rust", &at_lines);
    assert_eq!(at.len(), 10_000);
    assert!(at.iter().all(|line| line == &ansi("33", "42")));
    let over = code("rust", &(at_lines + "\n42"));
    assert_eq!(over.len(), 10_001);
    assert!(over.iter().all(|line| line == &ansi("38;5;195", "42")));

    // BMP non-ASCII text has multiple UTF-8 bytes, but counts once in .NET String.Length.
    let at_bytes = "42 ".to_owned() + &"é".repeat(512 * 1024 - 4);
    let at = MarkdownRenderer::render("", &format!("~~~rust\n{at_bytes}\n~~~"), 600_000, true);
    assert!(at[0].starts_with(&ansi("33", "42")));
    let over = MarkdownRenderer::render("", &format!("~~~rust\n{at_bytes}é\n~~~"), 600_000, true);
    assert_eq!(over, [ansi("38;5;195", &(at_bytes + "é"))]);
}

#[test]
#[ignore = "Legacy trailing whitespace handling"]
fn final_spacing_and_whitespace_only_code_lines_are_preserved_once() {
    assert_eq!(render("hello ", false), ["hello "]);
    assert_eq!(
        MarkdownRenderer::render("", "hello   ", 7, false),
        ["hello  "]
    );
    assert_eq!(render("one two", false), ["one two"]);
    assert_eq!(render("one two ", false), ["one two "]);
    assert_eq!(render("one two ", true), [ansi("38;5;195", "one two ")]);
    assert_eq!(render("~~~rust\n    \n42\n~~~", false), ["    ", "42"]);
    assert_eq!(
        code("rust", "    \n42"),
        [ansi("38;5;195", "    "), ansi("33", "42")]
    );
    assert_eq!(
        MarkdownRenderer::render("> ", "~~~rust\n    \n42\n~~~", 80, false),
        [">     ", "  42"]
    );
}

#[test]
#[ignore = "Legacy blockquote and nonbreaking-space rules"]
fn blockquote_strips_ascii_spaces_but_preserves_nbsp_until_wrapping() {
    let source = ">\u{a0}a";
    for width in [4, 5, 80] {
        assert_eq!(
            MarkdownRenderer::render("", source, width, false),
            ["│ \u{a0}a"]
        );
        assert_eq!(
            MarkdownRenderer::render("", source, width, true),
            [ansi("2;90", "│ ") + &ansi("38;5;195", "\u{a0}a")]
        );
    }
    assert_eq!(MarkdownRenderer::render("", source, 3, false), ["│", "a"]);
    assert_eq!(
        MarkdownRenderer::render("", source, 3, true),
        [ansi("2;90", "│"), ansi("38;5;195", "a")]
    );
    assert_eq!(
        MarkdownRenderer::render("- ", source, 5, false),
        ["- │", "  a"]
    );
    assert_eq!(
        MarkdownRenderer::render("- ", source, 5, true),
        [
            ansi("38;5;195", "- ") + &ansi("2;90", "│"),
            ansi("38;5;195", "  a")
        ]
    );
    assert_eq!(render(">   a", false), ["│ a"]);
    assert_eq!(render(">  \u{a0}a", false), ["│ \u{a0}a"]);
}

#[test]
#[ignore = "Legacy non-ASCII ordered markers and ANSI palette"]
fn ordered_lists_recognize_bmp_decimal_digits_using_utf8_marker_boundaries() {
    assert_eq!(
        render("١. item", true),
        [ansi("1;36", "١. ") + &ansi("38;5;195", "item")]
    );
    assert_eq!(
        MarkdownRenderer::render("", "١. item", 3, true),
        [
            ansi("1;36", "١. "),
            ansi("38;5;195", "ite"),
            ansi("38;5;195", "m")
        ]
    );
    assert_eq!(
        MarkdownRenderer::render("", "١. item", 5, true),
        [ansi("1;36", "١."), ansi("38;5;195", "item")]
    );
    assert_eq!(
        MarkdownRenderer::render("- ", "١. item", 8, true),
        [
            ansi("38;5;195", "- ") + &ansi("1;36", "١."),
            ansi("38;5;195", "  item")
        ]
    );
    assert_eq!(
        MarkdownRenderer::render("- ", "١. item", 8, false),
        ["- ١.", "  item"]
    );
    for marker in ["١٢. ", "१२) ", "１2. "] {
        assert_eq!(
            render(&(marker.to_owned() + "item"), true),
            [ansi("1;36", marker) + &ansi("38;5;195", "item")]
        );
    }
    // Number categories other than Nd, supplementary Nd, and non-ASCII separator spaces
    // do not satisfy the source's BMP char.IsDigit / literal-space marker grammar.
    for source in ["². item", "Ⅳ. item", "𝟙. item", "١.\u{a0}item", "١.item"] {
        assert_eq!(render(source, true), [ansi("38;5;195", source)]);
    }
}

#[test]
#[ignore = "Legacy tilde fences and language-token rules"]
fn fence_language_splits_only_ascii_spaces_after_trimming_outer_whitespace() {
    for info in ["rust\u{a0}foo", "rust\u{2003}foo", "rust\u{a0}\u{a0}foo"] {
        assert_eq!(code(info, "public"), [ansi("38;5;195", "public")]);
        let source = format!("```{info}\npublic\n```");
        assert_eq!(
            MarkdownRenderer::render("", &source, 1, true),
            ["p", "u", "b", "l", "i", "c"].map(|s| ansi("38;5;195", s))
        );
        assert_eq!(
            MarkdownRenderer::render("- ", &source, 5, true),
            [ansi("38;5;195", "- pub"), ansi("38;5;195", "  lic")]
        );
        assert_eq!(
            MarkdownRenderer::render("- ", &source, 5, false),
            ["- pub", "  lic"]
        );
    }
    for info in [
        "rust foo",
        "  rust  foo",
        "\u{a0}rust\u{a0}",
        "\u{2003}rust\u{2003}",
    ] {
        assert_eq!(code(info, "public"), [ansi("1;35", "public")]);
    }
}
