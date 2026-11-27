use std::sync::Arc;

use crate::common::{Cancelled, Names, NeverCancelled, TempDir};
use parrot_storage::artifacts::output_blobs::{
    OutputPreview, ToolOutputBlobStore, spill_limit_text, tool_output_preview,
};

fn names(values: &[&str]) -> Arc<Names> {
    Arc::new(Names::new(values.iter().copied()))
}

#[test]
fn tool_output_budget_counts_utf8_bytes_and_collision_link_text_is_exact() {
    let exact = format!("{}😀", "a".repeat(64 * 1024 - 4));
    assert_eq!(exact.len(), 64 * 1024);

    let root_dir = TempDir::new("tool-output");
    let root = root_dir.root();
    let directory = root.path().join("blobs");
    std::fs::create_dir_all(&directory).unwrap();
    let collision = directory.join("quiet-tree-arse.dat");
    std::fs::write(&collision, "existing").unwrap();
    let store = ToolOutputBlobStore::for_session(
        &root,
        "blobs",
        names(&["quiet-tree-arse.dat", "bold-river-arse.dat"]),
    )
    .unwrap();
    assert!(!store.is_oversized(&exact));
    assert!(store.is_oversized(&(exact.clone() + "é")));
    let output = "large 😀 output";
    let path_reference = store.persist(output, &NeverCancelled).unwrap();
    let path = directory.join("bold-river-arse.dat");
    assert_eq!(std::fs::read_to_string(&collision).unwrap(), "existing");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), output);
    assert_eq!(path_reference, path.display().to_string());
    assert!(path.is_absolute());
}

#[test]
fn configured_spill_limits_drive_the_oversized_check_and_notice_prefixes() {
    assert_eq!(spill_limit_text(64 * 1024), "64 KiB");
    assert_eq!(spill_limit_text(2 * 1024), "2 KiB");
    assert_eq!(spill_limit_text(4096), "4 KiB");
    assert_eq!(spill_limit_text(3000), "3000 B");

    let root_dir = TempDir::new("tool-output-limit");
    let root = root_dir.root();
    let store =
        ToolOutputBlobStore::for_session(&root, "agent/blobs", names(&["bold-river-arse.dat"]))
            .unwrap()
            .with_path_variables(std::collections::BTreeMap::from([(
                "AGENT_DIR".into(),
                root.path().display().to_string(),
            )]));
    let oversized = "x".repeat(4 * 1024 + 1);
    assert!(!store.is_oversized(&oversized));
    let custom = store.with_spill_limits(4 * 1024, 8 * 1024);
    assert!(custom.is_oversized(&oversized));
    assert_eq!(custom.tool_spill_limit_text(), "4 KiB");

    // Both notice prefixes derive from the configured limits, and a notice
    // written with any other limit is left untouched.
    let tool_notice = format!(
        "Process exited with code 0 after 0.01s\nTool output exceeded {} and was saved to {}/agent/blobs/bold-river-arse.dat.",
        spill_limit_text(4 * 1024),
        root.path().display(),
    );
    let process_notice =
        tool_notice.replace(&spill_limit_text(4 * 1024), &spill_limit_text(8 * 1024));
    let foreign_notice = tool_notice.replace(&spill_limit_text(4 * 1024), "15 KiB");
    let expected = "Process exited with code 0 after 0.01s\nTool output exceeded {} and was saved to $AGENT_DIR/agent/blobs/bold-river-arse.dat.";
    assert_eq!(
        custom.substitute_process_notice(&tool_notice),
        expected.replace("{}", "4 KiB")
    );
    assert_eq!(
        custom.substitute_process_notice(&process_notice),
        expected.replace("{}", "8 KiB")
    );
    assert_eq!(
        custom.substitute_process_notice(&foreign_notice),
        foreign_notice
    );
}

#[test]
fn configured_tool_output_notices_use_the_longest_path_variable_and_preserve_bytes() {
    let root_dir = TempDir::new("tool-output-variables");
    let root = root_dir.root();
    let store =
        ToolOutputBlobStore::for_session(&root, "agent/blobs", names(&["bold-river-arse.dat"]))
            .unwrap()
            .with_path_variables(std::collections::BTreeMap::from([
                ("WORKDIR".into(), root.path().display().to_string()),
                (
                    "AGENT_DIR".into(),
                    root.path().join("agent").display().to_string(),
                ),
            ]));
    let text = "original output 😀";
    assert_eq!(
        store.persist(text, &NeverCancelled).unwrap(),
        "$AGENT_DIR/blobs/bold-river-arse.dat"
    );
    assert_eq!(
        std::fs::read_to_string(store.directory().join("bold-river-arse.dat")).unwrap(),
        text
    );
}

#[test]
fn process_notices_substitute_paths_without_rewriting_inline_output_or_sibling_paths() {
    let root_dir = TempDir::new("process-notice-variables");
    let store = ToolOutputBlobStore::for_session(&root_dir.root(), "blobs", names(&[]))
        .unwrap()
        .with_path_variables(std::collections::BTreeMap::from([
            ("AGENT_DIR".into(), "/session/main".into()),
            ("AGENT_SCRATCH_DIR".into(), "/session/main/scratch".into()),
        ]));
    for header in ["", "Shell process 'integrated-checks' completed.\n"] {
        for (path, expected) in [
            (
                "/session/main/blobs/output.dat",
                "$AGENT_DIR/blobs/output.dat",
            ),
            (
                "/session/main/scratch/output.dat",
                "$AGENT_SCRATCH_DIR/output.dat",
            ),
            (
                "/session/main-worker/blobs/output.dat",
                "/session/main-worker/blobs/output.dat",
            ),
            ("$AGENT_DIR/blobs/output.dat", "$AGENT_DIR/blobs/output.dat"),
        ] {
            let completion = format!("{header}Process exited with code 0 after 83.76s");
            assert_eq!(
                store.substitute_process_notice(&format!(
                    "{completion}\nTool output exceeded 64 KiB and was saved to {path}."
                )),
                format!("{completion}\nTool output exceeded 64 KiB and was saved to {expected}.")
            );
        }
    }
    for text in [
        "ordinary output /session/main/blobs/output.dat",
        "Tool output exceeded 64 KiB and was saved to /session/main/blobs/output.dat.",
        "Process exited with code 0 after 0.00s\n[stdout]\nTool output exceeded 64 KiB and was saved to /session/main/blobs/output.dat.",
        "Process exited with code 0 after 0.00s\n[stderr]\n/session/main/blobs/output.dat",
    ] {
        assert_eq!(store.substitute_process_notice(text), text);
    }
}

#[test]
fn process_notices_rewrite_paths_and_keep_preview_lines_that_follow() {
    let root_dir = TempDir::new("process-notice-preview");
    let store = ToolOutputBlobStore::for_session(&root_dir.root(), "blobs", names(&[]))
        .unwrap()
        .with_path_variables(std::collections::BTreeMap::from([(
            "AGENT_DIR".into(),
            "/session/main".into(),
        )]));
    let completion =
        "Shell process 'integrated-checks' completed.\nProcess exited with code 0 after 83.76s";
    let notice = "Tool output exceeded 64 KiB and was saved to /session/main/blobs/output.dat.";
    let preview = "Preview of saved output:\nfirst\n<3 lines omitted to save context>\nlast";
    assert_eq!(
        store.substitute_process_notice(&format!("{completion}\n{notice}\n{preview}")),
        format!(
            "{completion}\nTool output exceeded 64 KiB and was saved to $AGENT_DIR/blobs/output.dat.\n{preview}"
        )
    );
    assert_eq!(
        store.substitute_process_notice(&format!(
            "Process exited with code 0 after 1.00s\n{notice}\n{preview}"
        )),
        format!(
            "Process exited with code 0 after 1.00s\nTool output exceeded 64 KiB and was saved to $AGENT_DIR/blobs/output.dat.\n{preview}"
        )
    );
    // A notice lookalike inside the preview lines must not win over the real
    // notice, which keeps a valid completion line before it.
    let lookalike = "Tool output exceeded 64 KiB and was saved to /session/main/blobs/echo.dat.";
    assert_eq!(
        store.substitute_process_notice(&format!("{completion}\n{notice}\n{lookalike}\n{preview}")),
        format!(
            "{completion}\nTool output exceeded 64 KiB and was saved to $AGENT_DIR/blobs/output.dat.\n{lookalike}\n{preview}"
        )
    );
}

#[test]
fn spill_preview_shows_all_lines_within_ten_and_first_last_five_above() {
    fn preview(lines: usize) -> Option<OutputPreview> {
        tool_output_preview(
            &(0..lines)
                .map(|index| format!("line-{index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
    assert!(tool_output_preview("").is_none());
    assert!(tool_output_preview("single").is_none_or(|p| p.omitted == 0 && p.tail.is_empty()));
    let ten = preview(10).unwrap();
    assert_eq!(ten.head.lines().count(), 10);
    assert_eq!(ten.tail, "");
    assert_eq!(ten.omitted, 0);
    let eleven = preview(11).unwrap();
    assert_eq!(eleven.head, "line-0\nline-1\nline-2\nline-3\nline-4");
    assert_eq!(eleven.tail, "line-6\nline-7\nline-8\nline-9\nline-10");
    assert_eq!(eleven.omitted, 1);
}

#[test]
fn spill_preview_skips_assembled_candidates_over_the_byte_budget() {
    // A single wide line within the budget stays, and one more byte drops it.
    let wide = format!("{}😀", "a".repeat(8188));
    assert_eq!(wide.len(), 8192);
    assert!(tool_output_preview(&wide).is_some());
    assert!(tool_output_preview(&(wide.clone() + "é")).is_none());
    // The omission marker counts toward the budget: these five-plus-five
    // lines fit alone, but the candidate with the marker exceeds 8192 bytes.
    let head_tail_bytes = 2 * (5 * 817 + 4);
    assert!(head_tail_bytes <= 8192);
    assert!(head_tail_bytes + "\n<1 lines omitted to save context>\n".len() > 8192);
    let line = "a".repeat(815);
    let lines = (0..11)
        .map(|index| format!("{line}-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(tool_output_preview(&lines).is_none());
    // Shorter lines keep the candidate, marker included, within the budget.
    let short = (0..11)
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(tool_output_preview(&short).is_some());
    // Multibyte characters at the boundary count as whole characters.
    let boundary = "é".repeat(4096);
    assert_eq!(boundary.len(), 8192);
    assert!(tool_output_preview(&boundary).is_some());
    assert!(tool_output_preview(&format!("{boundary}é")).is_none());
}

#[test]
fn spill_notice_and_preview_templates_render_the_documented_text() {
    let environment = std::collections::BTreeMap::from([
        ("HOME".to_owned(), "/tmp/c/home".to_owned()),
        ("XDG_CONFIG_HOME".to_owned(), "/tmp/c/xdg/config".to_owned()),
        ("XDG_STATE_HOME".to_owned(), "/tmp/c/xdg/state".to_owned()),
        ("XDG_DATA_HOME".to_owned(), "/tmp/c/xdg/data".to_owned()),
        ("WORKDIR".to_owned(), "/tmp/c/workspace".to_owned()),
    ]);
    let (_, catalog) =
        parrot_foundation::template_catalog::PromptTemplateCatalog::from_content("", &environment)
            .unwrap();
    assert_eq!(
        catalog
            .render(
                "tool-result.spill-notice",
                &[
                    ("path".to_owned(), "/x/output.dat".into()),
                    ("limit".to_owned(), "64 KiB".into()),
                ],
                &NeverCancelled
            )
            .unwrap(),
        "Tool output exceeded 64 KiB and was saved to /x/output.dat."
    );
    assert_eq!(
        catalog
            .render(
                "tool-result.spill-preview",
                &[
                    ("head".to_owned(), "first".into()),
                    ("tail".to_owned(), "last".into()),
                    ("omitted".to_owned(), 3.into()),
                ],
                &NeverCancelled
            )
            .unwrap(),
        "Preview of saved output:\nfirst\n<3 lines omitted to save context>\nlast"
    );
    assert_eq!(
        catalog
            .render(
                "tool-result.spill-preview",
                &[
                    ("head".to_owned(), "all".into()),
                    ("tail".to_owned(), "".into()),
                    ("omitted".to_owned(), 0.into()),
                ],
                &NeverCancelled
            )
            .unwrap(),
        "Preview of saved output:\nall"
    );
}

#[test]
fn captured_preview_applies_the_same_budget_and_short_output_rule() {
    let head: Vec<String> = (0..3).map(|index| format!("head-{index}")).collect();
    let tail: Vec<String> = (0..3).map(|index| format!("tail-{index}")).collect();
    let short = OutputPreview::from_captured(&head, &tail, 3).unwrap();
    assert_eq!(short.head, "head-0\nhead-1\nhead-2");
    assert_eq!(short.tail, "");
    assert_eq!(short.omitted, 0);
    let eleven: Vec<String> = (0..11).map(|index| format!("line-{index:02}")).collect();
    let preview = OutputPreview::from_captured(&eleven, &tail, 11).unwrap();
    assert_eq!(preview.head, "line-00\nline-01\nline-02\nline-03\nline-04");
    assert_eq!(preview.tail, "tail-0\ntail-1\ntail-2");
    assert_eq!(preview.omitted, 1);
    assert!(OutputPreview::from_captured(&[], &[], 0).is_none());
}

#[test]
fn tool_output_cancellation_removes_create_new_file_and_retries_are_bounded() {
    let root_dir = TempDir::new("tool-output-cancel");
    let root = root_dir.root();
    let store =
        ToolOutputBlobStore::for_session(&root, "blobs", names(&["lost-cloud-arse.dat"])).unwrap();
    assert_eq!(
        store.persist("output", &Cancelled).unwrap_err().kind,
        parrot_storage::artifacts::ErrorKind::Cancelled
    );
    assert!(!store.directory().join("lost-cloud-arse.dat").exists());

    let attempts = (0..16)
        .map(|n| format!("collision-{n}-arse.dat"))
        .collect::<Vec<_>>();
    std::fs::create_dir_all(store.directory()).unwrap();
    for name in &attempts {
        std::fs::write(store.directory().join(name), b"preserve").unwrap();
    }
    let refs = attempts.iter().map(String::as_str).collect::<Vec<_>>();
    let exhausted = ToolOutputBlobStore::for_session(&root, "blobs", names(&refs)).unwrap();
    assert_eq!(
        exhausted.persist("x", &NeverCancelled).unwrap_err().kind,
        parrot_storage::artifacts::ErrorKind::Conflict
    );
    for name in attempts {
        assert_eq!(
            std::fs::read(store.directory().join(name)).unwrap(),
            b"preserve"
        );
    }
}

#[test]
fn generated_filename_is_consumed_as_an_exact_basename_without_path_normalization() {
    let root_dir = TempDir::new("basename");
    let root = root_dir.root();
    let generator = names(&["kept-case-arse.dat"]);
    let store = ToolOutputBlobStore::for_session(&root, "blobs", generator).unwrap();
    let path_reference = store.persist("x", &NeverCancelled).unwrap();
    assert_eq!(
        path_reference,
        root.path()
            .join("blobs/kept-case-arse.dat")
            .display()
            .to_string()
    );
}
