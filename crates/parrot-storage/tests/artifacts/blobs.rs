use std::path::Path;
use std::sync::Arc;

use crate::common::{Cancelled, Names, NeverCancelled, TempDir};
use parrot_storage::artifacts::agent_output_file::AgentOutputFile;
use parrot_storage::artifacts::output_blobs::{
    ProcessOutput, ProcessOutputBlobStore, SpilledOutput, SpilledOutputEncoding,
    ToolOutputBlobStore, format_process_output, is_tool_output_oversized,
};

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
fn process_result_format_uses_exact_inline_or_blob_link_text() {
    assert_eq!(
        format_process_output(7, 1234, "out", "err", None),
        "Process exited with code 7 after 1.23s\n[stdout]\nout\n[stderr]\nerr"
    );
    assert_eq!(
        format_process_output(
            7,
            1234,
            "ignored",
            "ignored",
            Some(Path::new("/chosen/session/file-arse.dat"))
        ),
        "Process exited with code 7 after 1.23s\nTool output exceeded 64 KiB and was saved to /chosen/session/file-arse.dat."
    );
    assert_eq!(
        format_process_output(0, -1, "", "", None),
        "Process exited with code 0 after 0.00s"
    );
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
fn process_output_preserves_completion_link_sections_spills_and_cancellation_cleanup() {
    let root_dir = TempDir::new("process-output");
    let root = root_dir.root();
    let directory = root.path().join("process");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("quiet-tree-arse.dat"), "existing").unwrap();
    let store = ProcessOutputBlobStore::for_session(
        &root,
        "process",
        names(&["quiet-tree-arse.dat", "bold-river-arse.dat"]),
    )
    .unwrap();
    let stdout = ProcessOutput::Inline("out");
    let stderr = ProcessOutput::Inline("err");
    let path = store
        .persist(7, 1234, &stdout, &stderr, &NeverCancelled)
        .unwrap();
    assert_eq!(path, directory.join("bold-river-arse.dat"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "Process exited with code 7 after 1.23s\n[stdout]\nout\n[stderr]\nerr"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("quiet-tree-arse.dat")).unwrap(),
        "existing"
    );

    let spill_path = directory.join("source-spill.tmp");
    std::fs::write(&spill_path, b"xxa\xc3\xa9bc-tail").unwrap();
    let spill = ProcessOutput::Spilled(SpilledOutput {
        path: &spill_path,
        byte_offset: 2,
        character_length: 4,
        encoding: SpilledOutputEncoding::Utf8,
    });
    let spill_store = ProcessOutputBlobStore::for_session(
        &root,
        "process",
        names(&["spill-arse.dat", "instant-arse.dat"]),
    )
    .unwrap();
    let spill_path_saved = spill_store
        .persist(0, 0, &spill, &ProcessOutput::Inline(""), &NeverCancelled)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(spill_path_saved).unwrap(),
        "Process exited with code 0 after 0.00s\n[stdout]\naébc"
    );
    let immediate = spill_store
        .persist_immediately(1, 2000, &ProcessOutput::Inline("out"))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(immediate).unwrap(),
        "Process exited with code 1 after 2.00s\n[stdout]\nout"
    );

    let cancel_store =
        ProcessOutputBlobStore::for_session(&root, "process", names(&["lost-cloud-arse.dat"]))
            .unwrap();
    assert_eq!(
        cancel_store
            .persist(0, 0, &stdout, &stderr, &Cancelled)
            .unwrap_err()
            .kind,
        parrot_storage::artifacts::ErrorKind::Cancelled
    );
    assert!(!directory.join("lost-cloud-arse.dat").exists());
}

#[test]
fn utf8_spills_stream_in_bounded_chunks_and_preserve_multibyte_boundaries() {
    let root_dir = TempDir::new("process-output-utf8");
    let root = root_dir.root();
    let text = format!("{}😀tail", "a".repeat(4095));
    let spill_path = root.path().join("utf8-spill.tmp");
    std::fs::write(&spill_path, text.as_bytes()).unwrap();
    let spill = ProcessOutput::Spilled(SpilledOutput {
        path: &spill_path,
        byte_offset: 0,
        character_length: text.encode_utf16().count(),
        encoding: SpilledOutputEncoding::Utf8,
    });
    let store =
        ProcessOutputBlobStore::for_session(&root, "process", names(&["utf8-arse.dat"])).unwrap();
    let path = store
        .persist(0, 0, &spill, &ProcessOutput::Inline(""), &NeverCancelled)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        format!("Process exited with code 0 after 0.00s\n[stdout]\n{text}")
    );
}

#[test]
fn truncated_utf8_spill_fails_and_removes_partial_blob() {
    let root_dir = TempDir::new("process-output-truncated-spill");
    let root = root_dir.root();
    let spill_path = root.path().join("spill.tmp");
    std::fs::write(&spill_path, "aé").unwrap();
    let spill = ProcessOutput::Spilled(SpilledOutput {
        path: &spill_path,
        byte_offset: 0,
        character_length: 3,
        encoding: SpilledOutputEncoding::Utf8,
    });
    let store =
        ProcessOutputBlobStore::for_session(&root, "process", names(&["spill-arse.dat"])).unwrap();

    assert_eq!(
        store
            .persist(0, 0, &spill, &ProcessOutput::Inline(""), &NeverCancelled)
            .unwrap_err()
            .kind,
        parrot_storage::artifacts::ErrorKind::InvalidData
    );
    assert!(!store.directory().join("spill-arse.dat").exists());
}

#[test]
fn utf16_spills_stream_in_bounded_chunks_and_preserve_surrogate_pairs() {
    let root_dir = TempDir::new("process-output-utf16");
    let root = root_dir.root();
    let text = format!("{}😀tail", "a".repeat(4095));
    let spill_path = root.path().join("utf16-spill.tmp");
    let bytes = text
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(&spill_path, bytes).unwrap();
    let spill = ProcessOutput::Spilled(SpilledOutput {
        path: &spill_path,
        byte_offset: 0,
        character_length: text.encode_utf16().count(),
        encoding: SpilledOutputEncoding::Utf16Le,
    });
    let store =
        ProcessOutputBlobStore::for_session(&root, "process", names(&["utf16-arse.dat"])).unwrap();
    let path = store
        .persist(0, 0, &spill, &ProcessOutput::Inline(""), &NeverCancelled)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        format!("Process exited with code 0 after 0.00s\n[stdout]\n{text}")
    );
}

#[test]
fn truncated_utf16_spill_fails_and_removes_partial_blob() {
    let root_dir = TempDir::new("process-output-truncated-utf16");
    let root = root_dir.root();
    let spill_path = root.path().join("utf16-spill.tmp");
    std::fs::write(&spill_path, [b'a', 0, b'b', 0]).unwrap();
    let spill = ProcessOutput::Spilled(SpilledOutput {
        path: &spill_path,
        byte_offset: 0,
        character_length: 3,
        encoding: SpilledOutputEncoding::Utf16Le,
    });
    let store =
        ProcessOutputBlobStore::for_session(&root, "process", names(&["utf16-arse.dat"])).unwrap();

    assert_eq!(
        store
            .persist(0, 0, &spill, &ProcessOutput::Inline(""), &NeverCancelled)
            .unwrap_err()
            .kind,
        parrot_storage::artifacts::ErrorKind::InvalidData
    );
    assert!(!store.directory().join("utf16-arse.dat").exists());
}

#[test]
fn agent_output_is_created_readable_and_reopened_as_append_only_selected_root_file() {
    let root_dir = TempDir::new("agent-output");
    let root = root_dir.root();
    let first = AgentOutputFile::for_session(&root, "agents/agent-1").unwrap();
    let path = root.path().join("agents/agent-1/output-arse.dat");
    assert_eq!(first.output_path(), path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    first.append("first turn €").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first turn €");
    first.close().unwrap();
    first.close().unwrap();
    let resumed = AgentOutputFile::for_session(&root, "agents/agent-1").unwrap();
    resumed.append("\nsecond turn").unwrap();
    resumed.close().unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "first turn €\nsecond turn"
    );
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
