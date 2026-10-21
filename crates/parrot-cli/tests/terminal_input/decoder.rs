use parrot_cli::terminal_input::{
    IncrementalEditor, PromptState, TerminalKey, TerminalKeyDecoder, TerminalKeyKind,
};

fn hex(value: &str) -> String {
    use std::fmt::Write;
    value
        .as_bytes()
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").unwrap();
            text
        })
}

fn keys(lines: &mut Vec<String>, id: &str, step: usize, value: Vec<TerminalKey>) {
    lines.push(format!(
        "D\t{id}\t{step}\t{}",
        value
            .iter()
            .map(|k| format!("{:?}:{}", k.kind, hex(&k.text)))
            .collect::<Vec<_>>()
            .join(";")
    ));
}

fn state(lines: &mut Vec<String>, id: &str, editor: &IncrementalEditor, submitted: Option<String>) {
    let prompt = editor.prompt();
    lines.push(format!(
        "E\t{id}\t{}\t{}\t{}\t{}\t{}\t{}",
        hex(&prompt.prefix),
        hex(&prompt.text),
        prompt.cursor,
        usize::from(editor.is_empty()),
        usize::from(editor.is_recalling()),
        submitted.map_or_else(|| "-".to_owned(), |v| hex(&v))
    ));
}

#[test]
fn independent_dotnet_decoder_and_scalar_editor_observations_match_exactly() {
    let mut lines = Vec::new();
    let sequences = [
        "\x1b[A",
        "\x1bOA",
        "\x1b[B",
        "\x1bOB",
        "\x1b[C",
        "\x1bOC",
        "\x1b[D",
        "\x1bOD",
        "\x1b[H",
        "\x1b[F",
        "\x1bOH",
        "\x1bOF",
        "\x1b[3~",
        "\x1b[13;2u",
        "\x1b[27;2;13~",
        "\x1b[Z",
    ];
    for (i, sequence) in sequences.iter().enumerate() {
        let mut decoder = TerminalKeyDecoder::new();
        for (j, byte) in sequence.bytes().enumerate() {
            keys(
                &mut lines,
                &format!("sequence-{i}"),
                j,
                decoder.feed(&[byte]),
            );
        }
        keys(
            &mut lines,
            &format!("sequence-{i}"),
            sequence.len(),
            decoder.flush(),
        );
    }
    for value in [1, 3, 4, 5, 8, 127, 9, 10, 13, 11, 24, 30, 0, 31] {
        keys(
            &mut lines,
            &format!("control-{value}"),
            0,
            TerminalKeyDecoder::new().feed(&[value]),
        );
    }
    let mut decoder = TerminalKeyDecoder::new();
    for (i, byte) in "a界😀e\u{301}".bytes().enumerate() {
        keys(&mut lines, "utf", i, decoder.feed(&[byte]));
    }
    let mut decoder = TerminalKeyDecoder::new();
    for (i, chunk) in ["\x1b[2", "00~a\r", "\nb\rc\t\u{1}😀", "\x1b[20", "1~z"]
        .iter()
        .enumerate()
    {
        keys(&mut lines, "paste", i, decoder.feed(chunk.as_bytes()));
    }
    let mut decoder = TerminalKeyDecoder::new();
    keys(&mut lines, "escape", 0, decoder.feed(&[27]));
    keys(&mut lines, "escape", 1, decoder.flush());
    keys(&mut lines, "escape", 2, decoder.feed(b"\x1b[99~x"));
    let mut decoder = TerminalKeyDecoder::new();
    keys(&mut lines, "reset", 0, decoder.feed(b"\x1b[200~partial"));
    decoder.reset();
    keys(&mut lines, "reset", 1, decoder.feed(b"ok"));
    keys(
        &mut lines,
        "invalid",
        0,
        TerminalKeyDecoder::new().feed(&[0xff, 0xff, 0xff, 0xff, 0xef, 0xbf, 0xbd]),
    );
    keys(
        &mut lines,
        "bound",
        0,
        TerminalKeyDecoder::new()
            .feed(format!("\x1b[200~{}😀\x1b[201~", "x".repeat(65535)).as_bytes()),
    );
    let mut editor = IncrementalEditor::new("> ", 6);
    use TerminalKeyKind::*;
    editor.apply(TerminalKey::with_text(Paste, "😀a\ne\u{301}界z"));
    state(&mut lines, "insert", &editor, None);
    editor.apply(TerminalKey::new(Left));
    editor.apply(TerminalKey::new(Backspace));
    state(&mut lines, "backspace", &editor, None);
    editor.apply(TerminalKey::new(Home));
    state(&mut lines, "home", &editor, None);
    editor.apply(TerminalKey::new(PromptStart));
    state(&mut lines, "start", &editor, None);
    editor.replace_range(0, 1, "👩‍💻").unwrap();
    state(&mut lines, "replace-range", &editor, None);
    let submitted = editor.apply(TerminalKey::new(Submit));
    state(&mut lines, "submit", &editor, submitted);
    editor.remember("😀old");
    editor.remember("new");
    editor.replace("draft");
    editor.recall();
    state(&mut lines, "recall1", &editor, None);
    editor.recall();
    state(&mut lines, "recall2", &editor, None);
    editor.next();
    state(&mut lines, "next1", &editor, None);
    editor.next();
    state(&mut lines, "draft", &editor, None);
    editor.replace("a\nb");
    editor.apply(TerminalKey::new(Home));
    editor.apply(TerminalKey::new(KillLine));
    state(&mut lines, "kill", &editor, None);
    editor.replace("\u{1c}\u{85} abc \u{85}");
    state(&mut lines, "controlfilter", &editor, None);
    let submitted = editor.apply(TerminalKey::new(Submit));
    state(&mut lines, "trim", &editor, submitted);
    assert_eq!(
        lines.join("\n") + "\n",
        include_str!("fixtures/observations.tsv")
    );
}

#[test]
fn prompt_snapshot_sanitizes_without_confusing_utf8_bytes_and_scalar_cursor() {
    let prompt = PromptState {
        prefix: ">\n\t\x1b".into(),
        text: "😀\t\x1b界\n".into(),
        cursor: 99,
    }
    .sanitize();
    assert_eq!(prompt.prefix, ">    ");
    assert_eq!(prompt.text, "😀    界\n");
    assert_eq!(prompt.cursor, 7);
    for (cursor, expected) in [(-1, 0), (1, 1), (99, 2)] {
        let state = PromptState {
            prefix: ">\n ".into(),
            text: "🙂\u{1}x".into(),
            cursor,
        };
        let sanitized = state.sanitize();
        assert_eq!(sanitized.cursor, expected);
        assert_eq!(sanitized.text, "🙂x");
        assert_eq!(sanitized.prefix, "> ");
        assert_eq!(state.cursor, cursor);
        assert_eq!(state.text, "🙂\u{1}x");
    }
}

#[test]
fn reset_discards_partial_utf8_and_incomplete_escape() {
    let mut decoder = TerminalKeyDecoder::new();
    assert!(decoder.feed(&[0xf0, 0x9f]).is_empty());
    decoder.reset();
    assert_eq!(
        decoder.feed(b"x"),
        vec![TerminalKey::with_text(TerminalKeyKind::Character, "x")]
    );
    assert!(decoder.feed(b"\x1b[").is_empty());
    decoder.reset();
    assert!(decoder.flush().is_empty());
}
