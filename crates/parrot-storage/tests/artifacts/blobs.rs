use std::sync::Arc;

use crate::common::{Cancelled, Names, NeverCancelled, TempDir};
use parrot_storage::artifacts::output_blobs::{ToolOutputBlobStore, is_tool_output_oversized};

fn names(values: &[&str]) -> Arc<Names> {
    Arc::new(Names::new(values.iter().copied()))
}

#[test]
fn tool_output_budget_counts_utf8_bytes_and_collision_link_text_is_exact() {
    let exact = format!("{}😀", "a".repeat(64 * 1024 - 4));
    assert_eq!(exact.len(), 64 * 1024);
    assert!(!is_tool_output_oversized(&exact));
    assert!(is_tool_output_oversized(&(exact.clone() + "é")));

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
    let output = "large 😀 output";
    let notice = store.persist(output, &NeverCancelled).unwrap();
    let path = directory.join("bold-river-arse.dat");
    assert_eq!(std::fs::read_to_string(&collision).unwrap(), "existing");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), output);
    assert_eq!(
        notice,
        format!(
            "Tool output exceeded 64 KiB and was saved to {}.",
            path.display()
        )
    );
    assert!(path.is_absolute());
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
        "Tool output exceeded 64 KiB and was saved to $AGENT_DIR/blobs/bold-river-arse.dat."
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
    let notice = store.persist("x", &NeverCancelled).unwrap();
    assert_eq!(
        notice,
        format!(
            "Tool output exceeded 64 KiB and was saved to {}.",
            root.path().join("blobs/kept-case-arse.dat").display()
        )
    );
}
