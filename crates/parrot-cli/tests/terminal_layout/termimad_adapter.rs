use parrot_cli::terminal_layout::{MarkdownRenderer, TerminalText};

fn plain(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            assert_eq!(chars.next(), Some('['));
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
                assert!(c.is_ascii_digit() || c == ';');
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn ordinary_markdown_uses_termimad_layout() {
    let source = "# Heading\n\n* one\n* two\n\n1. alpha\n1. beta";
    let expected = termimad::MadSkin::default()
        .text(source, Some(40))
        .to_string();
    let actual = MarkdownRenderer::render("", source, 40, true);
    assert_eq!(
        actual.iter().map(|line| plain(line)).collect::<Vec<_>>(),
        expected
            .strip_suffix('\n')
            .unwrap()
            .lines()
            .map(plain)
            .collect::<Vec<_>>()
    );
    assert!(actual.iter().any(|line| plain(line).contains("2. beta")));
}

#[test]
fn prefixes_are_not_markdown_and_continuations_reserve_their_width() {
    let lines = MarkdownRenderer::render("**> ", "alpha beta gamma delta", 13, false);
    assert!(lines.len() > 1);
    assert!(lines[0].starts_with("**> "));
    assert!(lines[1..].iter().all(|line| line.starts_with("    ")));
    assert!(lines.iter().all(|line| TerminalText::width(line) <= 13));
}

#[test]
fn source_controls_are_sanitized_before_library_formatting() {
    let source = "hello\x1b[2J\tthere\r\x07\n```rust\nlet x = \"\x1b[H\";\n```";
    let lines = MarkdownRenderer::render("\x1b[2K> ", source, 80, true);
    let text = lines
        .iter()
        .map(|line| plain(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("[2K> hello[2J    there"));
    assert!(text.contains("[H"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
}

#[test]
fn color_does_not_change_visible_layout() {
    let source = "# Heading\n**bold** and `code`\n\n| a | b |\n| - | - |\n| 界 | é |\n\n```rust\nlet x = 42;  \n```";
    for width in [0, 1, 2, 3, 8, 19, 80] {
        let colored = MarkdownRenderer::render("> ", source, width, true);
        let plain_lines = MarkdownRenderer::render("> ", source, width, false);
        assert_eq!(
            colored.iter().map(|line| plain(line)).collect::<Vec<_>>(),
            plain_lines
        );
        assert!(plain_lines.iter().all(|line| !line.contains('\x1b')));
    }
}

#[test]
fn narrow_widths_and_unicode_keep_graphemes_and_text() {
    for source in ["abc", "界界", "🇲🇾🇯🇵", "👨‍👩‍👧‍👦x", "e\u{301}x", "x\u{1612f}y"]
    {
        for width in [0, 1, 2, 3, 5] {
            let lines = MarkdownRenderer::render("", source, width, false);
            assert_eq!(lines.concat(), source, "width {width}");
            for line in lines {
                assert!(
                    TerminalText::width(&line) <= width.max(1)
                        || unicode_segmentation::UnicodeSegmentation::graphemes(
                            line.as_str(),
                            true
                        )
                        .count()
                            == 1,
                    "overwide row {line:?} at {width}"
                );
            }
        }
    }
    assert_eq!(MarkdownRenderer::render("", "🇲🇾🇯🇵", 3, false), ["🇲🇾", "🇯🇵"]);
    assert_eq!(MarkdownRenderer::render("", "👨‍👩‍👧‍👦x", 2, false), ["👨‍👩‍👧‍👦", "x"]);
    assert_eq!(MarkdownRenderer::render("", "👨**‍**👩x", 3, false), ["👨‍👩x"]);
    assert_eq!(
        MarkdownRenderer::render("long> ", "ab", 2, false),
        ["long> a", "      b"]
    );
}

#[test]
fn narrow_tables_keep_all_column_content() {
    let source = "| left | right |\n| - | - |\n| alpha | omega |";
    for width in [0, 1, 2, 3, 8] {
        let lines = MarkdownRenderer::render("", source, width, false);
        let text = lines.concat();
        for value in ["left", "right", "alpha", "omega"] {
            assert!(
                text.contains(value),
                "missing {value} at width {width}: {text:?}"
            );
        }
        assert!(
            lines
                .iter()
                .all(|line| TerminalText::width(line) <= width.max(1))
        );
    }
}

#[test]
fn syntect_highlights_existing_languages_and_aliases() {
    for token in [
        "rust",
        "typescript",
        "kotlin",
        "swift",
        "csharp",
        "c-sharp",
        "c#",
        "golang",
        "python3",
        "py3",
        "shell",
        "sh",
        "zsh",
    ] {
        let source = format!("```{token}\nlet value = 42;\n```");
        let colored = MarkdownRenderer::render("", &source, 80, true);
        assert!(colored[0].contains("\x1b[38;2;"), "{token}");
        assert_eq!(plain(&colored[0]), "let value = 42;");
    }
}

#[test]
fn highlighting_keeps_multiline_state_before_wrapping() {
    let source = "```rust\n/* comment\ncontinued */ let n = 2;\n```";
    let colored = MarkdownRenderer::render("", source, 80, true);
    let isolated = MarkdownRenderer::render("", "```rust\ncontinued */ let n = 2;\n```", 80, true);
    assert_ne!(colored[1], isolated[0]);
    let wrapped = MarkdownRenderer::render("", source, 5, true);
    assert_eq!(
        wrapped.iter().map(|line| plain(line)).collect::<String>(),
        "/* commentcontinued */ let n = 2;"
    );
    assert!(
        wrapped
            .iter()
            .all(|line| TerminalText::width(&plain(line)) <= 5)
    );
    assert!(wrapped.iter().all(|line| line.ends_with("\x1b[0m")));
}

#[test]
fn unknown_languages_empty_unclosed_and_oversized_blocks_remain_plain() {
    assert_eq!(
        MarkdownRenderer::render("", "```unknown\na  \n```", 80, true),
        ["a  "]
    );
    assert_eq!(MarkdownRenderer::render("", "```\n```", 80, false), [""]);
    assert_eq!(
        MarkdownRenderer::render("", "```rust\nlet x = 1;", 80, false),
        ["let x = 1;"]
    );
    let source = format!("```rust\n{}\n```", "x\n".repeat(10_001));
    let lines = MarkdownRenderer::render("", &source, 80, true);
    assert!(lines.iter().all(|line| !line.contains('\x1b')));
    assert_eq!(lines.iter().filter(|line| *line == "x").count(), 10_001);
    let long = "x".repeat(512 * 1024);
    let source = format!("```rust\n{long}\n```");
    let lines = MarkdownRenderer::render("", &source, 80, true);
    assert_eq!(lines.concat(), long);
    assert!(lines.iter().all(|line| !line.contains('\x1b')));
}
