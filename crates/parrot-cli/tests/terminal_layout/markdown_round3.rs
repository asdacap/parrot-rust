//! Round 3 custom-parser regressions against MarkdownRenderer.cs at
//! 5f27a8a92625ee31d8bec094e5c3e09550193781 (LineRuns, ListMarker, TryFence).
use parrot_cli::terminal_layout::MarkdownRenderer;

fn ansi(code: &str, text: &str) -> String {
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn colored(source: &str) -> Vec<String> {
    MarkdownRenderer::render("", source, 120, true)
}

#[test]
fn quote_nbsp_preserves_source_text_until_the_word_wrapper_breaks_it() {
    assert_eq!(
        MarkdownRenderer::render("", ">\u{a0}a", 3, false),
        ["│", "a"]
    );
    assert_eq!(
        MarkdownRenderer::render("", ">\u{a0}a", 3, true),
        [ansi("2;90", "│"), ansi("38;5;195", "a")]
    );
    for space in ['\u{a0}', '\u{2003}', '\u{202f}', '\u{3000}'] {
        let content = format!("{space}a");
        // TrimStart(' ') stops at the first non-ASCII space, even when followed by ASCII spaces.
        for source in [format!(">{content}"), format!(">   {content}")] {
            assert_eq!(
                MarkdownRenderer::render("", &source, 120, false),
                [format!("│ {content}")]
            );
            assert_eq!(
                colored(&source),
                [ansi("2;90", "│ ") + &ansi("38;5;195", &content)]
            );
        }
        assert_eq!(
            colored(&format!("> {space}  a")),
            [ansi("2;90", "│ ") + &ansi("38;5;195", &format!("{space}  a"))]
        );
    }
    assert_eq!(
        colored(">   a"),
        [ansi("2;90", "│ ") + &ansi("38;5;195", "a")]
    );
}

#[test]
fn ordered_marker_uses_bmp_decimal_digits_and_utf8_safe_rest_boundaries() {
    for marker in ["١. ", "١٢) ", "१२. ", "１2٣) ", "1. "] {
        let source = format!("{marker}**item**");
        assert_eq!(
            colored(&source),
            [ansi("1;36", marker) + &ansi("1;38;5;195", "item")],
            "{source:?}"
        );
        assert_eq!(
            MarkdownRenderer::render("", &source, 120, false),
            [format!("{marker}item")]
        );
    }
    // char.IsDigit(char) examines UTF-16 units, not Unicode scalar values:
    // supplementary Nd digits begin with a surrogate, which is not a digit.
    for source in [
        "𝟙. item",
        "𐒡) item",
        "1𝟙. item",
        "². item",
        "Ⅳ. item",
        "١.item",
        "١.\u{a0}item",
        "١)\u{2003}item",
        ". item",
        "١x. item",
    ] {
        assert_eq!(colored(source), [ansi("38;5;195", source)], "{source:?}");
    }
    // An empty item is allowed as long as the literal ASCII separator exists.
    assert_eq!(colored("١. "), [ansi("1;36", "١. ")]);
    assert_eq!(colored("١."), [ansi("38;5;195", "١.")]);
}

#[test]
fn fence_outer_whitespace_and_literal_ascii_separator_are_distinct() {
    for marker in ["```", "~~~"] {
        for info in [
            "rust\u{a0}foo",
            "rust\u{2003}foo",
            "rust\u{202f}foo",
            "rust\u{3000}foo",
            "rust\u{a0} foo",
            "rust \u{a0}foo",
            "\u{a0}rust\u{a0}",
            "\u{2003}rust\u{2003}",
            "  rust   foo  ",
            " {.rust} foo ",
        ] {
            let source = format!("{marker}{info}\npublic\n{marker}");
            let highlighted = matches!(
                info,
                "rust \u{a0}foo"
                    | "\u{a0}rust\u{a0}"
                    | "\u{2003}rust\u{2003}"
                    | "  rust   foo  "
                    | " {.rust} foo "
            );
            assert_eq!(
                colored(&source),
                [ansi(
                    if highlighted { "1;35" } else { "38;5;195" },
                    "public"
                )],
                "{source:?}"
            );
            assert_eq!(
                MarkdownRenderer::render("", &source, 120, false),
                ["public"],
                "{source:?}"
            );
        }
    }
}

#[test]
fn exact_round3_reviewer_counterexamples_match_production_rows() {
    let cases: serde_json::Value = serde_json::from_str(REVIEW_CASES).unwrap();
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 56);
    for case in cases {
        let input = &case["input"];
        let expected: Vec<&str> = case["source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_str().unwrap())
            .collect();
        assert_eq!(
            MarkdownRenderer::render(
                input["prefix"].as_str().unwrap(),
                input["source"].as_str().unwrap(),
                input["width"].as_u64().unwrap() as usize,
                input["color"].as_bool().unwrap(),
            ),
            expected,
            "review input: {input}"
        );
    }
}

// Exact source rows copied from the independent review's
// acceptance-layout-counterexamples.json; the old Rust outputs are intentionally omitted.
const REVIEW_CASES: &str = r##"[
{"input":{"op":"md","source":">\u00a0a","width":3,"prefix":"","color":false},"source":["\u2502","a"]},
{"input":{"op":"md","source":">\u00a0a","width":3,"prefix":"","color":true},"source":["\u001b[2;90m\u2502\u001b[0m","\u001b[38;5;195ma\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"","color":false},"source":["\u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"","color":true},"source":["\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"- ","color":false},"source":["- \u2502","  a"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[2;90m\u2502\u001b[0m","\u001b[38;5;195m  a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"  ","color":false},"source":["  \u2502","  a"]},
{"input":{"op":"md","source":">\u00a0a","width":5,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[2;90m\u2502\u001b[0m","\u001b[38;5;195m  a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"","color":false},"source":["\u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"","color":true},"source":["\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"- ","color":false},"source":["- \u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"  ","color":false},"source":["  \u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":8,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"","color":false},"source":["\u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"","color":true},"source":["\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"- ","color":false},"source":["- \u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"  ","color":false},"source":["  \u2502 \u00a0a"]},
{"input":{"op":"md","source":">\u00a0a","width":80,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[2;90m\u2502 \u001b[0m\u001b[38;5;195m\u00a0a\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":1,"prefix":"","color":true},"source":["\u001b[1;36m\u0661\u001b[0m","\u001b[1;36m.\u001b[0m","\u001b[38;5;195mi\u001b[0m","\u001b[38;5;195mt\u001b[0m","\u001b[38;5;195me\u001b[0m","\u001b[38;5;195mm\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":1,"prefix":"- ","color":true},"source":["\u001b[38;5;195m-\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":1,"prefix":"  ","color":true},"source":["\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":2,"prefix":"","color":true},"source":["\u001b[1;36m\u0661.\u001b[0m","\u001b[38;5;195mit\u001b[0m","\u001b[38;5;195mem\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":2,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":2,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":3,"prefix":"","color":true},"source":["\u001b[1;36m\u0661. \u001b[0m","\u001b[38;5;195mite\u001b[0m","\u001b[38;5;195mm\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":3,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":3,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661\u001b[0m","\u001b[38;5;195m  \u001b[0m\u001b[1;36m.\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  t\u001b[0m","\u001b[38;5;195m  e\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":5,"prefix":"","color":true},"source":["\u001b[1;36m\u0661.\u001b[0m","\u001b[38;5;195mitem\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":5,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[1;36m\u0661. \u001b[0m","\u001b[38;5;195m  ite\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":5,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661. \u001b[0m","\u001b[38;5;195m  ite\u001b[0m","\u001b[38;5;195m  m\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":8,"prefix":"","color":true},"source":["\u001b[1;36m\u0661. \u001b[0m\u001b[38;5;195mitem\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":8,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[1;36m\u0661.\u001b[0m","\u001b[38;5;195m  item\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":8,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661.\u001b[0m","\u001b[38;5;195m  item\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":80,"prefix":"","color":true},"source":["\u001b[1;36m\u0661. \u001b[0m\u001b[38;5;195mitem\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":80,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m\u001b[1;36m\u0661. \u001b[0m\u001b[38;5;195mitem\u001b[0m"]},
{"input":{"op":"md","source":"\u0661. item","width":80,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m\u001b[1;36m\u0661. \u001b[0m\u001b[38;5;195mitem\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":1,"prefix":"","color":true},"source":["\u001b[38;5;195mp\u001b[0m","\u001b[38;5;195mu\u001b[0m","\u001b[38;5;195mb\u001b[0m","\u001b[38;5;195ml\u001b[0m","\u001b[38;5;195mi\u001b[0m","\u001b[38;5;195mc\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":1,"prefix":"- ","color":true},"source":["\u001b[38;5;195m-\u001b[0m","\u001b[38;5;195m  p\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":1,"prefix":"  ","color":true},"source":["\u001b[38;5;195mp\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":2,"prefix":"","color":true},"source":["\u001b[38;5;195mpu\u001b[0m","\u001b[38;5;195mbl\u001b[0m","\u001b[38;5;195mic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":2,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- \u001b[0m","\u001b[38;5;195m  p\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":2,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  \u001b[0m","\u001b[38;5;195m  p\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":3,"prefix":"","color":true},"source":["\u001b[38;5;195mpub\u001b[0m","\u001b[38;5;195mlic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":3,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- p\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":3,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  p\u001b[0m","\u001b[38;5;195m  u\u001b[0m","\u001b[38;5;195m  b\u001b[0m","\u001b[38;5;195m  l\u001b[0m","\u001b[38;5;195m  i\u001b[0m","\u001b[38;5;195m  c\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":5,"prefix":"","color":true},"source":["\u001b[38;5;195mpubli\u001b[0m","\u001b[38;5;195mc\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":5,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- pub\u001b[0m","\u001b[38;5;195m  lic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":5,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  pub\u001b[0m","\u001b[38;5;195m  lic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":8,"prefix":"","color":true},"source":["\u001b[38;5;195mpublic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":8,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- public\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":8,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  public\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":80,"prefix":"","color":true},"source":["\u001b[38;5;195mpublic\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":80,"prefix":"- ","color":true},"source":["\u001b[38;5;195m- public\u001b[0m"]},
{"input":{"op":"md","source":"```rust\u00a0foo\npublic\n```","width":80,"prefix":"  ","color":true},"source":["\u001b[38;5;195m  public\u001b[0m"]}
]"##;
