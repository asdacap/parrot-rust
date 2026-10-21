use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use parrot_cli::{
    presenters::*,
    terminal_frames::{
        LiveBufferItem, LiveBufferRenderContext, LiveBufferRetention, ScrollbackItem,
        ScrollbackLayout, ScrollbackRenderContext,
    },
    terminal_layout::TerminalText,
};

fn context(columns: usize, color: bool) -> ScrollbackRenderContext {
    ScrollbackRenderContext {
        columns,
        color,
        inline_diff: true,
        previous_packing: None,
    }
}
fn scroll(item: &Presentation, columns: usize, color: bool) -> Vec<String> {
    ScrollbackItem::render(item, context(columns, color)).unwrap()
}
fn terminal(label: &str, block: ToolBlock) -> Presentation {
    Presentation::Tool(ToolReport::terminal(
        label,
        block,
        ToolTerminalStatus::Succeeded,
        ToolPresentationMetadata::default(),
    ))
}

#[test]
fn display_values_sanitize_and_bound_labels_lines_and_utf8_details() {
    let item = Presentation::Tool(ToolReport::terminal(
        "safe\x1b[31m\nignored",
        ToolBlock::from_details((0..12).map(|index| format!("line-{index}\x1b[2J"))),
        ToolTerminalStatus::Errored,
        ToolPresentationMetadata::default(),
    ));
    let rows = scroll(&item, 32768, false);
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0], "✗ safe[31m");
    assert_eq!(rows[9], "  .. 2 lines truncated.");
    assert!(!rows.join("|").contains('\x1b'));
    let live = Presentation::Tool(ToolReport::live(
        "live\x1b[31m\nignored",
        ToolBlock::from_text("detail\x1b[2J"),
        ToolPresentationMetadata::default(),
        0,
    ));
    let live = LiveBufferItem::render(
        &live,
        LiveBufferRenderContext {
            columns: 32768,
            color: false,
        },
    )
    .unwrap();
    assert_eq!(live.lines[0].text(), "⠋ live[31m");
    assert_eq!(live.lines[1].text(), "  detail[2J");
    assert_eq!(live.retention, LiveBufferRetention::Tail);
    assert!(live.caret.is_none());
}

#[test]
fn wide_utf8_details_and_labels_are_bounded_without_splitting_scalars() {
    let rows = scroll(
        &terminal("large", ToolBlock::from_text("界".repeat(8000))),
        32768,
        false,
    );
    assert_eq!(rows.last().unwrap(), "  .. 1 lines truncated.");
    assert!(rows.concat().len() <= 16 * 1024 + 32);
    let item = Presentation::Tool(ToolReport::live(
        "界".repeat(8000),
        ToolBlock::empty(),
        ToolPresentationMetadata::default(),
        0,
    ));
    let rows = LiveBufferItem::render(
        &item,
        LiveBufferRenderContext {
            columns: 32768,
            color: false,
        },
    )
    .unwrap();
    assert!(rows.lines[0].text().len() <= 1032);
    assert!(rows.lines[0].text().ends_with('…'));
    assert!(
        scroll(
            &terminal("wrap", ToolBlock::from_text("x".repeat(100))),
            4,
            false
        )
        .len()
            <= 10
    );
    assert_eq!(
        ToolDisplayText::truncate_utf8("a😀界", 5),
        ("a😀".into(), true)
    );
    assert_eq!(ToolDisplayText::truncate_utf8("😀", 3), ("".into(), true));
}

#[test]
fn details_budget_counts_tabs_controls_blank_lines_and_remaining_items() {
    assert_eq!(
        ToolDisplayText::details(&["a\tb\r\x1b\n\nlast".into()], 10),
        vec!["a    b", "", "last"]
    );
    assert_eq!(
        ToolDisplayText::details(&["one".into(), "two".into(), "three".into()], 2),
        vec!["one", ".. 2 lines truncated."]
    );
    assert_eq!(
        ToolDisplayText::layout_details(&["abcdefgh".into()], 2, 0, 10),
        Vec::<String>::new()
    );
}

#[test]
fn live_rendering_uses_source_spinner_styles_and_real_timer_closure_on_clone() {
    let clock = Arc::new(AtomicUsize::new(0));
    let timer_clock = Arc::clone(&clock);
    let mut report = ToolReport::live(
        "long command",
        ToolBlock::from_text("detail"),
        ToolPresentationMetadata {
            style: ToolPresentationStyle::Muted,
            ..Default::default()
        },
        0,
    );
    report.timer = Some(Arc::new(move || {
        ["0s", "12s", "2m05s", "1h02m03s"][timer_clock.load(Ordering::SeqCst)].into()
    }));
    let item = Presentation::Tool(report);
    for (index, timer) in ["0s", "12s", "2m05s", "1h02m03s"].iter().enumerate() {
        clock.store(index, Ordering::SeqCst);
        let unchanged_frame = LiveBufferItem::render(
            &item,
            LiveBufferRenderContext {
                columns: 80,
                color: false,
            },
        )
        .unwrap();
        assert!(
            unchanged_frame.lines[0]
                .text()
                .contains(&format!("({timer})"))
        );
        let animated = item.animate(index);
        let rendered = LiveBufferItem::render(
            &animated,
            LiveBufferRenderContext {
                columns: 80,
                color: true,
            },
        )
        .unwrap();
        assert!(rendered.lines[0].text().contains(&format!("({timer})")));
        assert_eq!(
            rendered.lines[0].style().start(),
            "\x1b[48;5;236m\x1b[38;5;245m"
        );
        assert_eq!(
            rendered.lines[1].style().start(),
            "\x1b[48;5;236m\x1b[38;5;252m"
        );
        assert!(
            rendered.lines[0]
                .text()
                .starts_with("⠋⠙⠹⠸".chars().nth(index).unwrap())
        );
    }
}

#[test]
fn status_output_queue_completed_input_retain_distinct_source_budgets() {
    let body = (0..70)
        .map(|index| format!("row-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = scroll(&terminal("text", ToolBlock::from_text(&body)), 120, false);
    let queue = scroll(&terminal("queue", ToolBlock::from_queue(&body)), 120, false);
    let completed = scroll(
        &terminal("completed", ToolBlock::from_completed_input(&body)),
        120,
        false,
    );
    let status = scroll(
        &terminal("status", ToolBlock::from_status(&body)),
        120,
        false,
    );
    assert_eq!(text.len(), 10);
    assert_eq!(queue.len(), 30);
    assert!(completed.len() > 30 && completed.len() <= 100);
    assert_eq!(status.len(), 71);
    assert!(status.last().unwrap().contains("row-69"));
    let styled_text = scroll(&terminal("text", ToolBlock::from_text("colored")), 80, true);
    let styled_output = scroll(
        &terminal("output", ToolBlock::from_output("plain")),
        80,
        true,
    );
    assert!(styled_text[1].contains("\x1b[32mcolored\x1b[0m"));
    assert_eq!(styled_output[1], "  plain");
}

#[test]
fn terminal_status_markers_and_source_success_override_are_not_generic_dumps() {
    for (status, marker, code) in [
        (ToolTerminalStatus::Succeeded, "✓", "\x1b[32m"),
        (ToolTerminalStatus::Cancelled, "■", "\x1b[38;5;245m"),
        (ToolTerminalStatus::Errored, "✗", "\x1b[31m"),
        (ToolTerminalStatus::ReportedFailure, "✗", "\x1b[31m"),
    ] {
        let item = Presentation::Tool(ToolReport::terminal(
            "label",
            ToolBlock::empty(),
            status,
            ToolPresentationMetadata::default(),
        ));
        assert_eq!(
            scroll(&item, 80, true),
            vec![format!("{code}{marker} label\x1b[0m")]
        );
        assert_eq!(item.layout(), ScrollbackLayout::Compact);
    }
    let metadata = ToolPresentationMetadata {
        style: ToolPresentationStyle::Muted,
        success_icon: "○".into(),
        ..Default::default()
    };
    assert_eq!(
        scroll(
            &Presentation::Tool(ToolReport::terminal(
                "waiting",
                ToolBlock::empty(),
                ToolTerminalStatus::Succeeded,
                metadata
            )),
            80,
            true
        ),
        vec!["\x1b[38;5;245m○ waiting\x1b[0m"]
    );
}

#[test]
fn multiline_headers_keep_five_lines_then_exact_source_truncation_notice() {
    let label = (0..8)
        .map(|i| format!("command-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let metadata = ToolPresentationMetadata {
        multiline_label: true,
        ..Default::default()
    };
    let item = Presentation::Tool(ToolReport::terminal(
        label,
        ToolBlock::from_output("output"),
        ToolTerminalStatus::Succeeded,
        metadata,
    ));
    let rows = scroll(&item, 80, false);
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0], "✓ command-0");
    assert_eq!(rows[4], "  command-4");
    assert_eq!(rows[5], "  .. 3 lines truncated.");
    assert_eq!(rows[6], "  output");
}

#[test]
fn spawn_scrollback_preserves_heading_metadata_prompt_and_no_result_dump() {
    let spawn = Presentation::AgentSpawn {
        name: "worker".into(),
        metadata: "agent · model · fork: empty".into(),
        prompt: "ship it\nnext".into(),
    };
    assert_eq!(
        scroll(&spawn, 32768, false),
        vec![
            "♟ Started worker",
            "  agent · model · fork: empty",
            "",
            "  ship it",
            "  next"
        ]
    );
    let plain = Presentation::AgentSpawn {
        name: "".into(),
        metadata: "".into(),
        prompt: "hello".into(),
    };
    assert_eq!(
        scroll(&plain, 80, false),
        vec!["♟ Started agent", "", "  hello"]
    );
    assert_eq!(spawn.layout(), ScrollbackLayout::Block);
    assert!(scroll(&spawn, 80, true)[0].contains("Started \x1b[1mworker\x1b[0m"));
}

fn diff_item(tool_name: &str, path: &str) -> Presentation {
    Presentation::Diff {
        tool_name: tool_name.into(),
        path: path.into(),
        diff: "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n".into(),
    }
}

#[test]
fn unified_diff_renders_normalized_hunks_and_line_numbered_inline_rows() {
    let rows = scroll(&diff_item("write", "file"), 80, false);
    assert_eq!(
        rows,
        vec!["✓ write file", "  @@ -1,1 +1,1 @@", "  1 -old", "  1 +new"]
    );
    let colored = scroll(&diff_item("edit", "file"), 80, true).join("|");
    assert!(colored.contains("\x1b[31m1 -old\x1b[0m"));
    assert!(colored.contains("\x1b[32m1 +new\x1b[0m"));
}

#[test]
fn wide_diff_pairs_removed_added_rows_and_narrow_diff_stays_inline() {
    let item = diff_item("edit", "file");
    let wide = ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            inline_diff: false,
            ..context(80, false)
        },
    )
    .unwrap();
    assert_eq!(wide.len(), 3);
    assert!(wide[2].contains("1 -old") && wide[2].contains(" │ 1 +new"));
    let narrow = ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            inline_diff: false,
            ..context(39, false)
        },
    )
    .unwrap();
    assert_eq!(narrow, scroll(&item, 39, false));
}

#[test]
fn diff_context_rows_advance_old_new_counters_and_preserve_trailing_header_text() {
    let diff =
        "--- a/file\n+++ b/file\n@@ -10,3 +20,4 @@ heading\n context\n-old\n+new\n+extra\n tail";
    let rows = scroll(&terminal("edit", ToolBlock::from_diff(diff)), 120, false);
    assert_eq!(
        rows,
        vec![
            "✓ edit",
            "  @@ -10,3 +20,4 @@",
            "  20  context",
            "  11 -old",
            "  21 +new",
            "  22 +extra",
            "  23  tail"
        ]
    );
}

#[test]
fn side_by_side_diff_pads_unmatched_changes_and_keeps_context_pairs() {
    let diff = "--- a/file\n+++ b/file\n@@ -10,3 +20,4 @@\n context\n-old\n+new\n+extra\n tail";
    let item = terminal("edit", ToolBlock::from_diff(diff));
    let rows = ScrollbackItem::render(
        &item,
        ScrollbackRenderContext {
            inline_diff: false,
            ..context(80, false)
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 6);
    assert!(rows[2].contains("10  context") && rows[2].contains(" │ 20  context"));
    assert!(rows[3].contains("11 -old") && rows[3].contains(" │ 21 +new"));
    assert!(rows[4].trim_start().starts_with("│ 22 +extra"));
    assert!(rows[5].contains("12  tail") && rows[5].contains(" │ 23  tail"));
    assert!(rows.iter().all(|row| TerminalText::width(row) <= 80));
}

#[test]
fn diff_packing_compares_tool_and_path_by_value_not_arc_identity() {
    let first = diff_item("write", "a");
    let same = diff_item("write", "a");
    let other_path = diff_item("write", "b");
    let other_tool = diff_item("edit", "a");
    let packing = first.packing_identity();
    assert!(same.packs(packing.as_ref()));
    assert!(other_path.packs(packing.as_ref()));
    assert!(!other_tool.packs(packing.as_ref()));
    assert!(!same.ends_layout());
    assert!(!same.continues(&first));
    let same_rows = ScrollbackItem::render(
        &same,
        ScrollbackRenderContext {
            previous_packing: packing.clone(),
            ..context(80, false)
        },
    )
    .unwrap();
    assert_eq!(same_rows[0], "  @@ -1,1 +1,1 @@");
    assert!(!same_rows.iter().any(|row| row.contains("write a")));
    let other_rows = ScrollbackItem::render(
        &other_path,
        ScrollbackRenderContext {
            previous_packing: packing,
            ..context(80, false)
        },
    )
    .unwrap();
    assert_eq!(other_rows[0], "  b");
    assert_eq!(other_rows[1], "  @@ -1,1 +1,1 @@");
}

#[test]
fn diff_chunk_reports_and_multiple_files_keep_structured_headings() {
    let diff = "Chunk 1 has 1 match.\n\n--- a/one\n+++ b/one\n@@ -1 +1 @@\n-old\n+new\n--- a/two\n+++ b/renamed\n@@ -3,0 +3,1 @@\n+added";
    let rows = scroll(&terminal("edit", ToolBlock::from_diff(diff)), 120, false);
    assert_eq!(
        rows,
        vec![
            "✓ edit",
            "  Chunk 1 has 1 match.",
            "  one",
            "  @@ -1,1 +1,1 @@",
            "  1 -old",
            "  1 +new",
            "  two → renamed",
            "  @@ -3,0 +3,1 @@",
            "  3 +added"
        ]
    );
}

#[test]
fn malformed_or_binary_diff_remains_raw_and_does_not_lose_lines() {
    for diff in [
        "not unified\n\tdata",
        "--- a/file\n+++ b/file\nBinary files differ",
        "--- a/file\n+++ b/file\n@@ bad @@\n-bad",
    ] {
        let rows = scroll(&terminal("write", ToolBlock::from_diff(diff)), 120, false);
        assert_eq!(rows.len(), diff.lines().count() + 1);
        assert_eq!(rows[1], format!("  {}", diff.lines().next().unwrap()));
    }
}

#[test]
fn diff_row_and_source_byte_limits_report_omissions_and_preserve_utf8() {
    let diff = format!(
        "--- a/file\n+++ b/file\n@@ -1,0 +1,120 @@\n{}",
        (0..120)
            .map(|i| format!("+line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let rows = scroll(&terminal("edit", ToolBlock::from_diff(diff)), 120, false);
    assert_eq!(rows.len(), 102);
    assert_eq!(rows.last().unwrap(), "  … 21 diff rows omitted");
    let source_limited = scroll(
        &terminal("edit", ToolBlock::from_diff("界".repeat(200_000))),
        120,
        false,
    );
    assert_eq!(
        source_limited.last().unwrap(),
        "  … additional diff text omitted"
    );
    assert!(
        source_limited
            .iter()
            .all(|line| TerminalText::width(line) <= 120)
    );
}

#[test]
fn out_of_range_structured_hunk_fails_instead_of_making_up_line_numbers() {
    let item = terminal(
        "edit",
        ToolBlock::from_diff("--- a/file\n+++ b/file\n@@ -2147483648 +1 @@\n-old\n+new"),
    );
    let error = ScrollbackItem::render(&item, context(80, false)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn decoration_reserves_source_content_columns_before_wrapping() {
    let decoration = ActivityDecoration::describe(30, 1, Some("child"), "♟");
    assert_eq!(decoration.width(), 14);
    assert_eq!(decoration.content_columns(30), 16);
    let item = terminal(
        "a message that wraps at content boundary",
        ToolBlock::empty(),
    );
    let rows = item
        .render_scrollback_decorated(context(30, false), &decoration)
        .unwrap();
    assert!(rows[0].starts_with("  ✓ [child] ♟ "));
    assert!(rows[1].starts_with("    [child]   "));
    assert!(rows.iter().all(|line| TerminalText::width(line) <= 30));
    let tiny = ActivityDecoration::describe(1, 1000, Some("long"), "♟");
    assert_eq!(tiny.content_columns(1), 1);
}

#[test]
fn trusted_lines_retain_real_markdown_ansi_and_layout() {
    let item = Presentation::Lines {
        lines: vec!["\x1b[32mtrusted\x1b[0m".into()],
        layout: ScrollbackLayout::Assistant,
    };
    assert_eq!(scroll(&item, 80, true), vec!["\x1b[32mtrusted\x1b[0m"]);
    assert_eq!(item.layout(), ScrollbackLayout::Assistant);
    assert!(item.is_completed());
}
