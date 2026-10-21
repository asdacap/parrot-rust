use parrot_cli::terminal_input::{
    EditorRangeError, IncrementalEditor, PromptState, TerminalKey, TerminalKeyKind,
};

fn press(editor: &mut IncrementalEditor, kind: TerminalKeyKind) {
    assert_eq!(editor.apply(TerminalKey::new(kind)), None);
}

fn type_text(editor: &mut IncrementalEditor, text: &str) {
    assert_eq!(
        editor.apply(TerminalKey::with_text(TerminalKeyKind::Character, text)),
        None
    );
}

#[test]
fn editor_applies_rune_aware_multiline_edits_and_limits_input() {
    let mut editor = IncrementalEditor::new("> ", 5);
    type_text(&mut editor, "a🙂c");
    press(&mut editor, TerminalKeyKind::Left);
    press(&mut editor, TerminalKeyKind::Newline);
    type_text(&mut editor, "xy");
    assert_eq!(editor.prompt().text, "a🙂\nxc");
    press(&mut editor, TerminalKeyKind::Home);
    press(&mut editor, TerminalKeyKind::KillLine);
    assert_eq!(
        editor.apply(TerminalKey::new(TerminalKeyKind::Submit)),
        Some("a🙂".into())
    );
    assert_eq!(
        editor.prompt(),
        PromptState {
            prefix: "> ".into(),
            text: String::new(),
            cursor: 0
        }
    );
    assert!(editor.is_empty());
}

#[test]
fn editor_replaces_text_rune_aware_at_its_input_limit() {
    let mut editor = IncrementalEditor::new("> ", 3);
    editor.replace("a🙂b\u{1b}[2Jc");
    assert_eq!(editor.prompt().text, "a🙂b");
    assert_eq!(editor.prompt().cursor, 3);
    let mut zero = IncrementalEditor::new("", 0);
    type_text(&mut zero, "🙂");
    press(&mut zero, TerminalKeyKind::Newline);
    assert!(zero.is_empty());
    assert_eq!(zero.prompt().cursor, 0);
}

#[test]
fn editor_replaces_only_a_rune_range_and_leaves_the_caret_before_the_suffix() {
    let mut editor = IncrementalEditor::new("> ", 20);
    editor.replace("🙂 $al suffix");
    editor.replace_range(2, 3, "$alpha").unwrap();
    assert_eq!(editor.prompt().text, "🙂 $alpha suffix");
    assert_eq!(editor.prompt().cursor, 8);
}

#[test]
fn replacement_ranges_use_scalars_and_preserve_suffix_at_capacity() {
    let mut editor = IncrementalEditor::new("", 5);
    editor.replace("🙂e\u{301}xy");
    editor.replace_range(1, 2, "\t界\u{1b}🦜z").unwrap();
    assert_eq!(editor.prompt().text, "🙂界🦜xy");
    assert_eq!(editor.prompt().cursor, 3);
    editor.replace_range(5, 0, "ignored").unwrap();
    assert_eq!(editor.prompt().cursor, 5);
    editor.replace_range(0, 5, "").unwrap();
    assert!(editor.is_empty());
}

#[test]
fn invalid_ranges_leave_text_cursor_and_recall_untouched() {
    let mut editor = IncrementalEditor::new("", 8);
    editor.remember("🙂ab");
    editor.recall();
    press(&mut editor, TerminalKeyKind::Left);
    let before = editor.prompt();
    for (start, length) in [(4, 0), (2, 2), (usize::MAX, 0), (0, usize::MAX)] {
        assert_eq!(
            editor.replace_range(start, length, "x"),
            Err(EditorRangeError)
        );
        assert_eq!(editor.prompt(), before);
        assert!(editor.is_recalling());
    }
}

#[test]
fn editor_recalls_submitted_entries_and_edits_away_from_them() {
    let mut editor = IncrementalEditor::new("> ", 64 * 1024);
    editor.remember("first");
    editor.remember("second");
    editor.remember("   ");
    editor.recall();
    assert_eq!(editor.prompt().text, "second");
    assert_eq!(editor.prompt().cursor, 6);
    assert!(editor.is_recalling());
    editor.recall();
    assert_eq!(editor.prompt().text, "first");
    editor.recall();
    assert_eq!(editor.prompt().text, "first");
    editor.next();
    assert_eq!(editor.prompt().text, "second");
    type_text(&mut editor, "x");
    assert!(!editor.is_recalling());
    assert_eq!(editor.prompt().text, "secondx");
    type_text(&mut editor, "y");
    editor.recall();
    assert_eq!(editor.prompt().text, "second");
    editor.next();
    editor.next();
    assert_eq!(editor.prompt().text, "secondxy");
    assert!(!editor.is_recalling());
    let mut empty = IncrementalEditor::new("", 10);
    empty.recall();
    empty.next();
    assert!(empty.is_empty());
}

#[test]
fn history_preserves_entries_and_recalls_them_through_filter_and_limit() {
    let mut editor = IncrementalEditor::new("", 4);
    editor.remember("\t\n\u{85}\u{a0}\u{1680}\u{2003}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}");
    editor.recall();
    assert!(!editor.is_recalling());
    editor.remember(" 🙂\u{1b}abc ");
    editor.remember(" 🙂\u{1b}abc ");
    editor.recall();
    assert_eq!(editor.prompt().text, " 🙂ab");
    editor.recall();
    assert!(editor.is_recalling());
    editor.next();
    assert!(editor.is_recalling());
    editor.next();
    assert!(editor.is_empty());
}

#[test]
fn clear_and_replace_text_preserve_recall_but_replace_ends_it() {
    let mut editor = IncrementalEditor::new("", 20);
    editor.replace("draft🙂");
    editor.remember("entry");
    editor.recall();
    editor.clear();
    assert!(editor.is_empty());
    assert!(editor.is_recalling());
    editor.replace_text("temporary");
    assert!(editor.is_recalling());
    editor.next();
    assert_eq!(editor.prompt().text, "draft🙂");
    assert_eq!(editor.prompt().cursor, 6);
    editor.recall();
    editor.replace("replacement");
    assert!(!editor.is_recalling());
    editor.next();
    assert_eq!(editor.prompt().text, "replacement");
    editor.recall();
    editor.replace_range(0, 0, "").unwrap();
    assert!(!editor.is_recalling());
}

#[test]
fn home_moves_to_line_start_while_prompt_start_moves_to_prompt_start() {
    let mut editor = IncrementalEditor::new("", 20);
    editor.replace("first\nsecond");
    press(&mut editor, TerminalKeyKind::Home);
    assert_eq!(editor.prompt().cursor, 6);
    press(&mut editor, TerminalKeyKind::PromptStart);
    assert_eq!(editor.prompt().cursor, 0);
    press(&mut editor, TerminalKeyKind::Left);
    assert_eq!(editor.prompt().cursor, 0);
    press(&mut editor, TerminalKeyKind::End);
    assert_eq!(editor.prompt().cursor, 5);
    press(&mut editor, TerminalKeyKind::Right);
    press(&mut editor, TerminalKeyKind::End);
    assert_eq!(editor.prompt().cursor, 12);
    press(&mut editor, TerminalKeyKind::Right);
    assert_eq!(editor.prompt().cursor, 12);
}

#[test]
fn combining_marks_are_independent_scalars_and_non_bmp_deletion_is_atomic() {
    let mut editor = IncrementalEditor::new("", 20);
    editor.replace("🙂e\u{301}🦜");
    assert_eq!(editor.prompt().cursor, 4);
    press(&mut editor, TerminalKeyKind::Left);
    press(&mut editor, TerminalKeyKind::Backspace);
    assert_eq!(editor.prompt().text, "🙂e🦜");
    assert_eq!(editor.prompt().cursor, 2);
    press(&mut editor, TerminalKeyKind::Delete);
    assert_eq!(editor.prompt().text, "🙂e");
    press(&mut editor, TerminalKeyKind::Home);
    press(&mut editor, TerminalKeyKind::Delete);
    assert_eq!(editor.prompt().text, "e");
    assert_eq!(editor.prompt().cursor, 0);
}

#[test]
fn kill_line_removes_suffix_then_newline_but_not_following_line() {
    let mut editor = IncrementalEditor::new("", 30);
    editor.replace("a🙂\nbc\nlast");
    press(&mut editor, TerminalKeyKind::PromptStart);
    press(&mut editor, TerminalKeyKind::Right);
    press(&mut editor, TerminalKeyKind::KillLine);
    assert_eq!(editor.prompt().text, "a\nbc\nlast");
    press(&mut editor, TerminalKeyKind::KillLine);
    assert_eq!(editor.prompt().text, "abc\nlast");
    assert_eq!(editor.prompt().cursor, 1);
    press(&mut editor, TerminalKeyKind::End);
    press(&mut editor, TerminalKeyKind::Right);
    press(&mut editor, TerminalKeyKind::End);
    press(&mut editor, TerminalKeyKind::KillLine);
    assert_eq!(editor.prompt().text, "abc\nlast");
}

#[test]
fn no_op_and_edit_keys_have_source_recall_transitions() {
    for kind in [
        TerminalKeyKind::Up,
        TerminalKeyKind::Down,
        TerminalKeyKind::Complete,
        TerminalKeyKind::Escape,
        TerminalKeyKind::Interrupt,
        TerminalKeyKind::Mode,
        TerminalKeyKind::Right,
        TerminalKeyKind::End,
        TerminalKeyKind::Delete,
    ] {
        let mut editor = IncrementalEditor::new("", 1);
        editor.remember("x");
        editor.recall();
        press(&mut editor, kind);
        assert!(editor.is_recalling(), "{kind:?}");
    }
    let mut editor = IncrementalEditor::new("", 1);
    editor.remember("x");
    editor.recall();
    press(&mut editor, TerminalKeyKind::PromptStart);
    press(&mut editor, TerminalKeyKind::Backspace);
    assert!(editor.is_recalling());
    for kind in [
        TerminalKeyKind::Character,
        TerminalKeyKind::Paste,
        TerminalKeyKind::Newline,
        TerminalKeyKind::KillLine,
        TerminalKeyKind::EndOfFile,
    ] {
        editor.recall();
        press(&mut editor, TerminalKeyKind::End);
        press(&mut editor, kind);
        assert!(!editor.is_recalling(), "{kind:?}");
    }
    editor.recall();
    editor.clear();
    press(&mut editor, TerminalKeyKind::EndOfFile);
    assert!(editor.is_recalling());
}

#[test]
fn end_of_file_deletes_at_cursor_without_submitting() {
    let mut editor = IncrementalEditor::new("", 8);
    editor.replace("🙂ab");
    press(&mut editor, TerminalKeyKind::PromptStart);
    press(&mut editor, TerminalKeyKind::EndOfFile);
    assert_eq!(editor.prompt().text, "ab");
    assert_eq!(editor.prompt().cursor, 0);
    press(&mut editor, TerminalKeyKind::End);
    press(&mut editor, TerminalKeyKind::EndOfFile);
    assert_eq!(editor.prompt().text, "ab");
}

#[test]
fn insertion_filters_dotnet_control_scalars_but_preserves_newlines_and_format_characters() {
    let mut editor = IncrementalEditor::new("", 400);
    let controls: String = (0..=0x9f)
        .filter_map(char::from_u32)
        .filter(|c| c.is_control())
        .collect();
    type_text(&mut editor, &controls);
    assert_eq!(editor.prompt().text, "\n");
    assert_eq!(editor.prompt().cursor, 1);
    editor.replace("a\r\nb\tc\u{1b}[2J\u{200d}\u{200b}\u{feff}\u{2028}");
    assert_eq!(
        editor.prompt().text,
        "a\nbc[2J\u{200d}\u{200b}\u{feff}\u{2028}"
    );
}

#[test]
fn multiline_paste_and_newline_remain_until_submit() {
    let mut editor = IncrementalEditor::new("", 30);
    assert_eq!(
        editor.apply(TerminalKey::with_text(
            TerminalKeyKind::Paste,
            "first\nsecond"
        )),
        None
    );
    assert_eq!(editor.prompt().text, "first\nsecond");
    press(&mut editor, TerminalKeyKind::Newline);
    type_text(&mut editor, "third");
    assert_eq!(
        editor.apply(TerminalKey::new(TerminalKeyKind::Submit)),
        Some("first\nsecond\nthird".into())
    );
}

#[test]
fn submit_uses_dotnet_whitespace_and_resets_draft_without_remembering_automatically() {
    let mut editor = IncrementalEditor::new("", 100);
    editor.replace("draft");
    editor.remember("entry");
    editor.recall();
    editor.replace_text("\u{a0}\u{1680}\u{2000}\u{2028}🙂e\u{301}\u{2029}\u{202f}\u{205f}\u{3000}");
    assert_eq!(
        editor.apply(TerminalKey::new(TerminalKeyKind::Submit)),
        Some("🙂e\u{301}".into())
    );
    assert!(editor.is_empty());
    assert!(!editor.is_recalling());
    editor.recall();
    assert_eq!(editor.prompt().text, "entry");
    editor.next();
    assert!(editor.is_empty());
    assert_eq!(
        editor.apply(TerminalKey::new(TerminalKeyKind::Submit)),
        Some(String::new())
    );
    editor.replace("\u{200b}\u{feff} x \u{feff}\u{200b}");
    assert_eq!(
        editor.apply(TerminalKey::new(TerminalKeyKind::Submit)),
        Some("\u{200b}\u{feff} x \u{feff}\u{200b}".into())
    );
}

#[test]
fn prompt_is_an_owned_unsanitized_snapshot() {
    let mut editor = IncrementalEditor::new(">\n ", 20);
    editor.replace("🙂x");
    let snapshot = editor.prompt();
    editor.clear();
    assert_eq!(
        snapshot,
        PromptState {
            prefix: ">\n ".into(),
            text: "🙂x".into(),
            cursor: 2
        }
    );
    assert_eq!(
        editor.prompt(),
        PromptState {
            prefix: ">\n ".into(),
            text: String::new(),
            cursor: 0
        }
    );
}
