use std::collections::BTreeMap;

use parrot_domain::contracts::{
    async_ops::OperationError,
    filesystem::{
        EntryKind, FileReadError, FileReadErrorKind, LinkPolicy, MutationRequest, MutationTarget,
        ReadRequest,
    },
    security::InvocationContext,
    tools::{ParallelSafety, Tool},
};
use parrot_tools::files::{
    EditTool, FileToolError, FileToolInvocation, GlobTool, ReadTool, WorkspacePaths, WriteTool,
};
use serde_json::json;

use super::fixtures::*;

#[tokio::test]
async fn mutation_validates_expanded_empty_paths_before_cancellation_or_io() {
    let service = RecordingService::default();
    let paths = WorkspacePaths::new(&BTreeMap::from([("EMPTY".into(), String::new())]));
    let write = WriteTool::new(service.clone(), paths.clone(), definition());
    let edit = EditTool::new(service.clone(), paths, definition());
    let written = write
        .execute(
            &invocation(json!({"path":"$EMPTY", "content":"x"})),
            &Snapshot(1),
            &AlreadyCancelled,
        )
        .await
        .unwrap();
    let edited = edit
        .execute(
            &invocation(json!({"path":"${EMPTY}", "old_string":"", "new_string":"x"})),
            &Snapshot(1),
            &AlreadyCancelled,
        )
        .await
        .unwrap();
    assert_eq!(
        written.text,
        "error: Tool argument 'path' must not be empty."
    );
    assert_eq!(edited.text, written.text);
    assert!(service.calls().is_empty());
}

#[tokio::test]
async fn read_reports_requested_range_empty_and_total_after_byte_truncation() {
    let service = RecordingService::default();
    service.file(
        "lines.txt",
        "/actual/lines.txt",
        b"alpha\nbeta\ngamma\n".to_vec(),
    );
    service.file("empty.txt", "/actual/empty.txt", Vec::new());
    service.file(
        "large.txt",
        "/actual/large.txt",
        format!("{}\n", "x".repeat(600)).repeat(2001).into_bytes(),
    );
    let tool = ReadTool::new(service, WorkspacePaths::default(), definition());
    let range = tool
        .execute(
            &invocation(json!({"path":"lines.txt","offset":2,"limit":1})),
            &Snapshot(7),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(range.text, "2: beta\ntotal lines in file: 3\n");
    assert!(!range.text.contains("sha256:"));
    let empty = tool
        .execute(
            &invocation(json!({"path":"empty.txt"})),
            &Snapshot(7),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(empty.text, "total lines in file: 0\n");
    let large = tool
        .execute(
            &invocation(json!({"path":"large.txt","limit":2000})),
            &Snapshot(7),
            &NoCancel,
        )
        .await
        .unwrap();
    assert!(
        large
            .text
            .ends_with("[output truncated]\ntotal lines in file: 2001\n")
    );
    assert!(large.text.starts_with(&format!("1: {}\n", "x".repeat(600))));
    assert!(!large.text.contains("14: "));
}

#[tokio::test]
async fn read_markdown_outline_depends_on_bound_actual_extension_and_partial_range() {
    let content = b"# Title\nintro\n## Section\n```\n# not a heading\n```\n#hashtag\n### Sub\n";
    for (physical, offset, limit, expect_outline) in [
        ("doc.md", 1, 2, true),
        ("doc.md", 3, 100, true),
        ("doc.md", 1, 100, false),
        ("doc.txt", 1, 2, false),
    ] {
        let service = RecordingService::default();
        service.file("alias.txt", physical, content.to_vec());
        let tool = ReadTool::new(service, WorkspacePaths::default(), definition());
        let result = tool
            .execute(
                &invocation(json!({"path":"alias.txt","offset":offset,"limit":limit})),
                &Snapshot(8),
                &NoCancel,
            )
            .await
            .unwrap();
        let outline = "total lines in file: 8\nmarkdown outline (line: heading):\n1: # Title\n3: ## Section\n8: ### Sub\n";
        assert_eq!(result.text.ends_with(outline), expect_outline);
        assert_eq!(result.text.contains("markdown outline"), expect_outline);
    }
}

#[tokio::test]
async fn read_dispatches_original_expanded_path_expected_target_and_captured_snapshot() {
    let service = RecordingService::default();
    service.file(
        "/scratch/alias.txt",
        "/observed/actual.txt",
        b"live output".to_vec(),
    );
    let paths = WorkspacePaths::new(&BTreeMap::from([("SCRATCH".into(), "/scratch".into())]));
    let tool = ReadTool::new(service.clone(), paths, definition());
    let result = tool
        .invoke(
            invocation(json!({"path":"${SCRATCH}/alias.txt"})),
            InvocationContext::new(Snapshot(42), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "1: live output\ntotal lines in file: 1\n");
    let calls = service.calls();
    assert_eq!(
        calls.iter().map(|call| &call.action).collect::<Vec<_>>(),
        vec![
            &Action::Inspect("/scratch/alias.txt".into(), LinkPolicy::Follow),
            &Action::Read(ReadRequest {
                requested: "/scratch/alias.txt".into(),
                expected_physical: Some("/observed/actual.txt".into()),
                links: LinkPolicy::Follow,
                max_bytes: None
            }),
        ]
    );
    assert_shared_snapshot(&calls, 42);
    assert_eq!(
        tool.parallel_safety(&invocation(json!({}))),
        ParallelSafety::ParallelSafe
    );
    assert!(!tool.enabled_after_interruption());
}

#[tokio::test]
async fn read_propagates_second_authorized_operation_failure_without_rendering_bytes() {
    let service = RecordingService::default();
    service.file("alias", "/actual", b"secret".to_vec());
    let error = FileReadError::new(
        FileReadErrorKind::TargetChanged,
        "target changed on second authorization",
    );
    service.0.lock().unwrap().reject_read = Some(error.clone());
    let tool = ReadTool::new(service.clone(), WorkspacePaths::default(), definition());
    let result = tool
        .invoke(
            invocation(json!({"path":"alias"})),
            InvocationContext::new(Snapshot(43), (), &NoCancel),
        )
        .await;
    assert_eq!(
        result,
        Err(OperationError::Failed(FileToolError::File(error)))
    );
    assert_eq!(service.calls().len(), 2);
    assert_shared_snapshot(&service.calls(), 43);
}

#[tokio::test]
async fn read_detects_nul_in_probe_and_decodes_stream_reader_bom_and_replacement() {
    for (bytes, expected) in [
        (b"a\0b".to_vec(), "error: binary file"),
        (
            vec![0xef, 0xbb, 0xbf, b'a'],
            "1: a\ntotal lines in file: 1\n",
        ),
        (vec![0xff, 0xfe, b'a', 0], "error: binary file"),
        (vec![b'a', 0xff], "1: a�\ntotal lines in file: 1\n"),
    ] {
        let service = RecordingService::default();
        service.file("file", "file", bytes);
        let tool = ReadTool::new(service, WorkspacePaths::default(), definition());
        let result = tool
            .execute(&invocation(json!({"path":"file"})), &Snapshot(1), &NoCancel)
            .await
            .unwrap();
        assert_eq!(result.text, expected);
    }
}

#[tokio::test]
async fn strict_invalid_arguments_never_dispatch_to_services() {
    let read_arguments = [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"path":null}"#,
        r#"{"path":1}"#,
        r#"{"path":"x","extra":true}"#,
        r#"{"path":"x","offset":0}"#,
        r#"{"path":"x","offset":1.5}"#,
        r#"{"path":"x","limit":0}"#,
        r#"{"path":"x","limit":2001}"#,
    ];
    let write_arguments = [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"path":null,"content":"x"}"#,
        r#"{"path":1,"content":"x"}"#,
        r#"{"path":"x","content":false}"#,
        r#"{"path":"","content":"x"}"#,
        r#"{"path":"x"}"#,
        r#"{"path":"x","content":"x","extra":true}"#,
    ];
    let edit_arguments = [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"path":null,"old_string":"old","new_string":"new"}"#,
        r#"{"path":"x","old_string":"","new_string":"y"}"#,
        r#"{"path":"x","old_string":"x","new_string":null}"#,
        r#"{"path":"x","old_string":"x","new_string":"y","replace_all":"false"}"#,
        r#"{"path":"x","old_string":"x","new_string":"y","extra":true}"#,
        r#"{"path":"x","old_string":"\u0000","new_string":"y"}"#,
        r#"{"path":"x","old_string":"x","new_string":"\u0000"}"#,
    ];
    let glob_arguments = [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"pattern":null}"#,
        r#"{"pattern":1}"#,
        r#"{"pattern":"x","extra":true}"#,
        r#"{"pattern":""}"#,
        r#"{"pattern":"../x"}"#,
        r#"{"pattern":"/absolute"}"#,
        r#"{"pattern":"\u0000"}"#,
    ];
    let service = RecordingService::default();
    service.0.lock().unwrap().mutation_bytes = Some(b"old".to_vec());
    let read = ReadTool::new(service.clone(), WorkspacePaths::default(), definition());
    let write = WriteTool::new(service.clone(), WorkspacePaths::default(), definition());
    let edit = EditTool::new(service.clone(), WorkspacePaths::default(), definition());
    let glob = GlobTool::new(service.clone(), definition());
    for arguments in read_arguments {
        assert!(
            read.execute(
                &FileToolInvocation::new("invalid", arguments),
                &Snapshot(1),
                &NoCancel
            )
            .await
            .unwrap()
            .text
            .starts_with("error: "),
            "read: {arguments}"
        );
    }
    for arguments in write_arguments {
        assert!(
            write
                .execute(
                    &FileToolInvocation::new("invalid", arguments),
                    &Snapshot(1),
                    &NoCancel
                )
                .await
                .unwrap()
                .text
                .starts_with("error: "),
            "write: {arguments}"
        );
    }
    for arguments in edit_arguments {
        assert!(
            edit.execute(
                &FileToolInvocation::new("invalid", arguments),
                &Snapshot(1),
                &NoCancel
            )
            .await
            .unwrap()
            .text
            .starts_with("error: "),
            "edit: {arguments}"
        );
    }
    for arguments in glob_arguments {
        assert!(
            glob.execute(
                &FileToolInvocation::new("invalid", arguments),
                &Snapshot(1),
                &NoCancel
            )
            .await
            .unwrap()
            .text
            .starts_with("error: "),
            "glob: {arguments}"
        );
    }
    assert!(service.calls().is_empty());
    assert_eq!(
        service.0.lock().unwrap().mutation_bytes.as_deref(),
        Some(b"old".as_slice())
    );
}

#[tokio::test]
async fn read_and_glob_preserve_rejected_root_errors_and_do_not_traverse() {
    for glob in [false, true] {
        let service = RecordingService::default();
        service.0.lock().unwrap().inspect_errors.insert(
            "/alias".into(),
            FileReadError::new(FileReadErrorKind::InvalidOperation, "access denied"),
        );
        let result = if glob {
            GlobTool::new(service.clone(), definition())
                .execute(
                    &invocation(json!({"path":"/alias", "pattern":"**"})),
                    &Snapshot(1),
                    &NoCancel,
                )
                .await
        } else {
            ReadTool::new(service.clone(), WorkspacePaths::default(), definition())
                .execute(
                    &invocation(json!({"path":"/alias"})),
                    &Snapshot(1),
                    &NoCancel,
                )
                .await
        }
        .unwrap();
        assert_eq!(result.text, "error: access denied");
        assert_eq!(service.calls().len(), 1);
    }
}

#[tokio::test]
async fn external_read_paths_and_filtered_directory_reports_are_not_reinterpreted_as_workspace_only()
 {
    let service = RecordingService::default();
    service.file(
        "/external/outside.txt",
        "/external/outside.txt",
        b"outside".to_vec(),
    );
    service.file(
        "../external/outside.txt",
        "/external/outside.txt",
        b"outside".to_vec(),
    );
    let read = ReadTool::new(service.clone(), WorkspacePaths::default(), definition());
    for path in ["/external/outside.txt", "../external/outside.txt"] {
        assert_eq!(
            read.execute(&invocation(json!({"path":path})), &Snapshot(1), &NoCancel)
                .await
                .unwrap()
                .text,
            "1: outside\ntotal lines in file: 1\n"
        );
    }
    service.directory(
        "/external",
        vec![
            metadata(
                "/external/visible.txt",
                "/external/visible.txt",
                EntryKind::Regular,
            ),
            metadata(
                "/external/hidden.txt",
                "/external/hidden.txt",
                EntryKind::Regular,
            ),
        ],
    );
    service.0.lock().unwrap().inspect_errors.insert(
        "/external/hidden.txt".into(),
        FileReadError::new(FileReadErrorKind::InvalidOperation, "access denied"),
    );
    assert_eq!(
        read.execute(
            &invocation(json!({"path":"/external"})),
            &Snapshot(1),
            &NoCancel
        )
        .await
        .unwrap()
        .text,
        "visible.txt\n"
    );
    let glob = GlobTool::new(service.clone(), definition());
    assert_eq!(
        glob.execute(
            &invocation(json!({"path":"/external", "pattern":"**"})),
            &Snapshot(1),
            &NoCancel
        )
        .await
        .unwrap()
        .text,
        "visible.txt\n"
    );
    service.0.lock().unwrap().inspect_errors.insert(
        "/external/visible.txt".into(),
        FileReadError::new(FileReadErrorKind::InvalidOperation, "access denied"),
    );
    assert_eq!(
        read.execute(
            &invocation(json!({"path":"/external"})),
            &Snapshot(1),
            &NoCancel
        )
        .await
        .unwrap()
        .text,
        ""
    );
    assert_eq!(
        glob.execute(
            &invocation(json!({"path":"/external", "pattern":"**"})),
            &Snapshot(1),
            &NoCancel
        )
        .await
        .unwrap()
        .text,
        ""
    );
}

#[tokio::test]
async fn directory_listing_sorts_ordinally_filters_failed_entries_and_marks_directories() {
    let service = RecordingService::default();
    service.directory(
        "dir",
        vec![
            metadata("dir/z", "dir/z", EntryKind::Regular),
            metadata("dir/😀", "dir/😀", EntryKind::Regular),
            metadata("dir/\u{e000}", "dir/\u{e000}", EntryKind::Regular),
            metadata("dir/missing", "dir/missing", EntryKind::Regular),
            metadata("dir/A", "dir/A", EntryKind::Directory),
        ],
    );
    service.0.lock().unwrap().inspect_errors.insert(
        "dir/missing".into(),
        FileReadError::new(FileReadErrorKind::NotFound, "vanished"),
    );
    let tool = ReadTool::new(service.clone(), WorkspacePaths::default(), definition());
    let result = tool
        .invoke(
            invocation(json!({"path":"dir"})),
            InvocationContext::new(Snapshot(44), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "A/\nz\n😀\n\u{e000}\n");
    assert_shared_snapshot(&service.calls(), 44);
    assert!(
        service
            .calls()
            .iter()
            .all(|call| !matches!(call.action, Action::Read(_)))
    );
}

#[tokio::test]
async fn directory_listing_cap_counts_utf16_units_not_utf8_bytes_and_allows_last_line_overshoot() {
    let service = RecordingService::default();
    let a = format!("a{}", "😀".repeat(3000));
    let b = format!("b{}", "😀".repeat(1100));
    let entries = [a.as_str(), b.as_str(), "c"]
        .into_iter()
        .map(|name| {
            let path = format!("dir/{name}");
            metadata(&path, &path, EntryKind::Regular)
        })
        .collect();
    service.directory("dir", entries);
    let tool = ReadTool::new(service, WorkspacePaths::default(), definition());
    let result = tool
        .execute(&invocation(json!({"path":"dir"})), &Snapshot(1), &NoCancel)
        .await
        .unwrap();
    assert_eq!(result.text, format!("{a}\n{b}\n[listing truncated]\n"));
}

#[tokio::test]
async fn directory_listing_line_cap_is_2000() {
    let service = RecordingService::default();
    service.directory(
        "dir",
        (0..2001)
            .map(|index| {
                let name = char::from_u32(0x1000 + index).unwrap();
                let path = format!("dir/{name}");
                metadata(&path, &path, EntryKind::Regular)
            })
            .collect(),
    );
    let tool = ReadTool::new(service, WorkspacePaths::default(), definition());
    let result = tool
        .execute(&invocation(json!({"path":"dir"})), &Snapshot(1), &NoCancel)
        .await
        .unwrap();
    assert!(result.text.ends_with("[listing truncated]\n"));
    assert_eq!(result.text.lines().count(), 2001);
    assert_eq!(
        result.text.encode_utf16().count(),
        4000 + "[listing truncated]\n".len()
    );
}

#[tokio::test]
async fn write_emits_exact_utf8_without_bom_replaces_bom_and_skips_identical_bytes() {
    for (before, content, before_lines, after_lines) in [
        (None, "héllo\r\n世界\n", 0, 2),
        (
            Some(vec![
                0xef, 0xbb, 0xbf, b'a', b'\r', b'\n', b'b', b'\r', b'\n',
            ]),
            "a\nb",
            2,
            2,
        ),
        (None, "", 0, 0),
    ] {
        let service = RecordingService::default();
        service.0.lock().unwrap().mutation_bytes = before;
        let tool = WriteTool::new(service.clone(), WorkspacePaths::default(), definition());
        let result = tool
            .execute(
                &invocation(json!({"path":"nested/unicode.txt","content":content})),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(
            result.text,
            format!("Lines before: {before_lines}\nLines after: {after_lines}")
        );
        assert_eq!(
            service.0.lock().unwrap().mutation_bytes.as_deref(),
            Some(content.as_bytes())
        );
        let calls = service.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].action,
            Action::MutationWrite(
                MutationRequest {
                    requested: "nested/unicode.txt".into(),
                    create: true
                },
                content.as_bytes().into()
            )
        );
        let unchanged = tool
            .execute(
                &invocation(json!({"path":"nested/unicode.txt","content":content})),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(
            unchanged.text,
            format!("Lines before: {after_lines}\nLines after: {after_lines}")
        );
        assert_eq!(service.calls().len(), 3);
        assert!(matches!(service.calls()[2].action, Action::MutationRead(_)));
    }
}

#[tokio::test]
async fn write_counts_logical_lines_for_empty_and_newline_edges() {
    for (before, content, before_lines, after_lines) in [
        (b"".as_slice(), "", 0, 0),
        (b"one\n", "one", 1, 1),
        (b"one\r\ntwo\r\n", "one\n\nthree\n", 2, 3),
        (b"one\rtwo\r", "one\rtwo\r", 1, 1),
        (b"\n\n", "\n", 2, 1),
        (b"one", "", 1, 0),
        (b"\xff\n\xfe", "\0\n", 2, 1),
    ] {
        let service = RecordingService::default();
        service.0.lock().unwrap().mutation_bytes = Some(before.to_vec());
        let tool = WriteTool::new(service, WorkspacePaths::default(), definition());
        let result = tool
            .execute(
                &invocation(json!({"path":"lines.txt","content":content})),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(
            result.text,
            format!("Lines before: {before_lines}\nLines after: {after_lines}")
        );
    }
}

#[tokio::test]
async fn mutations_repeat_original_requested_path_and_snapshot_and_edit_renders_final_target_name()
{
    for edit_mode in [false, true] {
        let service = RecordingService::default();
        {
            let mut state = service.0.lock().unwrap();
            state.mutation_bytes = Some(b"before\n".to_vec());
            state.mutation_read_target = Some(MutationTarget {
                physical_path: "/old-target".into(),
                display_path: "old-display".into(),
            });
            state.mutation_write_target = Some(MutationTarget {
                physical_path: "/actual/final".into(),
                display_path: "actual-final.txt".into(),
            });
        }
        let paths = WorkspacePaths::new(&BTreeMap::from([("ROOT".into(), "/lexical".into())]));
        let result = if edit_mode {
            EditTool::new(service.clone(), paths, definition())
                .invoke(
                    invocation(
                        json!({"path":"$ROOT/alias","old_string":"before","new_string":"after"}),
                    ),
                    InvocationContext::new(Snapshot(45), (), &NoCancel),
                )
                .await
        } else {
            WriteTool::new(service.clone(), paths, definition())
                .invoke(
                    invocation(json!({"path":"$ROOT/alias","content":"after\n"})),
                    InvocationContext::new(Snapshot(45), (), &NoCancel),
                )
                .await
        }
        .unwrap();
        if edit_mode {
            assert!(result.text.contains("--- a/actual-final.txt"));
            assert!(result.text.contains("+++ b/actual-final.txt"));
            assert!(!result.text.contains("old-display"));
        } else {
            assert_eq!(result.text, "Lines before: 1\nLines after: 1");
        }
        let calls = service.calls();
        assert_eq!(calls.len(), 2);
        let request = MutationRequest {
            requested: "/lexical/alias".into(),
            create: !edit_mode,
        };
        assert_eq!(calls[0].action, Action::MutationRead(request.clone()));
        assert_eq!(
            calls[1].action,
            Action::MutationWrite(request, b"after\n".to_vec())
        );
        assert_shared_snapshot(&calls, 45);
    }
}

#[tokio::test]
async fn later_invocations_use_their_new_snapshot_and_preserve_bytes_after_reported_denial() {
    let service = RecordingService::default();
    let tool = WriteTool::new(service.clone(), WorkspacePaths::default(), definition());
    let first = tool
        .invoke(
            invocation(json!({"path":"/external/new.txt", "content":"new"})),
            InvocationContext::new(Snapshot(1), (), &NoCancel),
        )
        .await
        .unwrap();
    assert!(!first.text.starts_with("error:"));
    service.0.lock().unwrap().reject_read = Some(FileReadError::new(
        FileReadErrorKind::InvalidOperation,
        "Write access denied",
    ));
    let second = tool
        .invoke(
            invocation(json!({"path":"/external/new.txt", "content":"changed"})),
            InvocationContext::new(Snapshot(2), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(second.text, "error: Write access denied");
    assert_eq!(
        service.0.lock().unwrap().mutation_bytes.as_deref(),
        Some(b"new".as_slice())
    );
    assert_eq!(
        service
            .calls()
            .iter()
            .map(|call| call.snapshot_id)
            .collect::<Vec<_>>(),
        [1, 1, 2]
    );
}

#[tokio::test]
async fn first_mutation_validation_failures_are_formatted_without_any_write() {
    for kind in [
        FileReadErrorKind::Denied,
        FileReadErrorKind::InvalidOperation,
        FileReadErrorKind::Io,
        FileReadErrorKind::NotFound,
        FileReadErrorKind::InvalidPath,
    ] {
        for edit in [false, true] {
            let service = RecordingService::default();
            {
                let mut state = service.0.lock().unwrap();
                state.mutation_bytes = Some(b"old".to_vec());
                state.reject_read =
                    Some(FileReadError::new(kind, "fixture rejected actual target"));
            }
            let request = if edit {
                json!({"path":"/synthetic/external/file", "old_string":"old", "new_string":"new"})
            } else {
                json!({"path":"/synthetic/external/file", "content":"new"})
            };
            let result = if edit {
                EditTool::new(service.clone(), WorkspacePaths::default(), definition())
                    .execute(&invocation(request), &Snapshot(55), &NoCancel)
                    .await
            } else {
                WriteTool::new(service.clone(), WorkspacePaths::default(), definition())
                    .execute(&invocation(request), &Snapshot(55), &NoCancel)
                    .await
            }
            .unwrap();
            assert_eq!(result.text, "error: fixture rejected actual target");
            let calls = service.calls();
            assert_eq!(calls.len(), 1);
            assert!(
                matches!(&calls[0].action, Action::MutationRead(request) if request.requested == std::path::Path::new("/synthetic/external/file") && request.create != edit)
            );
            assert_eq!(
                service.0.lock().unwrap().mutation_bytes.as_deref(),
                Some(b"old".as_slice())
            );
        }
    }
}

#[tokio::test]
async fn second_mutation_service_authorization_failure_returns_error_and_preserves_before_bytes() {
    for edit_mode in [false, true] {
        let service = RecordingService::default();
        {
            let mut state = service.0.lock().unwrap();
            state.mutation_bytes = Some(b"old".to_vec());
            state.reject_write = Some(FileReadError::new(
                FileReadErrorKind::Denied,
                "second authorization denied",
            ));
        }
        let result = if edit_mode {
            EditTool::new(service.clone(), WorkspacePaths::default(), definition())
                .execute(
                    &invocation(json!({"path":"x","old_string":"old","new_string":"new"})),
                    &Snapshot(46),
                    &NoCancel,
                )
                .await
        } else {
            WriteTool::new(service.clone(), WorkspacePaths::default(), definition())
                .execute(
                    &invocation(json!({"path":"x","content":"new"})),
                    &Snapshot(46),
                    &NoCancel,
                )
                .await
        }
        .unwrap();
        assert_eq!(result.text, "error: second authorization denied");
        assert_eq!(
            service.0.lock().unwrap().mutation_bytes.as_deref(),
            Some(b"old".as_slice())
        );
        assert_eq!(service.calls().len(), 2);
        assert_shared_snapshot(&service.calls(), 46);
    }
}

#[tokio::test]
async fn edit_match_counts_default_single_replace_all_zero_and_identical_output_do_not_write() {
    for (before, old, new, replace_all, expected) in [
        (
            "one one",
            "one",
            "two",
            None,
            "error: Expected exactly one old_string match, but found 2.",
        ),
        (
            "one one",
            "missing",
            "two",
            None,
            "error: Expected exactly one old_string match, but found 0.",
        ),
        ("one one", "missing", "two", Some(true), "No changes made."),
        ("bb A", "bb", "bb", Some(false), "No changes made."),
        ("A", "a", "x", Some(true), "No changes made."),
    ] {
        let service = RecordingService::default();
        service.0.lock().unwrap().mutation_bytes = Some(before.as_bytes().to_vec());
        let tool = EditTool::new(service.clone(), WorkspacePaths::default(), definition());
        let mut arguments = json!({"path":"file","old_string":old,"new_string":new});
        if let Some(value) = replace_all {
            arguments["replace_all"] = json!(value);
        }
        let result = tool
            .execute(&invocation(arguments), &Snapshot(1), &NoCancel)
            .await
            .unwrap();
        assert_eq!(result.text, expected);
        assert_eq!(service.calls().len(), 1);
        assert_eq!(
            service.0.lock().unwrap().mutation_bytes.as_deref(),
            Some(before.as_bytes())
        );
    }
}

#[tokio::test]
async fn edit_ordinal_nonoverlapping_multiline_empty_replacement_and_bom_crlf_are_verbatim() {
    for (before, old, new, all, expected) in [
        (b"aaaa A".to_vec(), "aa", "b", true, b"bb A".to_vec()),
        (
            b"start\r\nfirst\nsecond\rend".to_vec(),
            "first\nsecond",
            "ONE\r\nTWO",
            false,
            b"start\r\nONE\r\nTWO\rend".to_vec(),
        ),
        (
            vec![0xef, 0xbb, 0xbf, b'a', 13, 10, b'b', 13, b'c', 10],
            "b",
            "β",
            false,
            vec![0xef, 0xbb, 0xbf, b'a', 13, 10, 0xce, 0xb2, 13, b'c', 10],
        ),
        (
            b"before-middle-after".to_vec(),
            "middle",
            "",
            false,
            b"before--after".to_vec(),
        ),
    ] {
        let service = RecordingService::default();
        service.0.lock().unwrap().mutation_bytes = Some(before);
        let tool = EditTool::new(service.clone(), WorkspacePaths::default(), definition());
        let result = tool
            .execute(
                &invocation(
                    json!({"path":"file","old_string":old,"new_string":new,"replace_all":all}),
                ),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert!(!result.text.starts_with("error: "));
        assert_eq!(service.0.lock().unwrap().mutation_bytes, Some(expected));
        assert_eq!(service.calls().len(), 2);
    }
}

#[tokio::test]
async fn edit_rejects_invalid_utf8_and_nul_without_dispatching_write() {
    for bytes in [vec![0xff, 0xfe, 0x41], vec![0x61, 0, 0x62]] {
        let service = RecordingService::default();
        service.0.lock().unwrap().mutation_bytes = Some(bytes.clone());
        let tool = EditTool::new(service.clone(), WorkspacePaths::default(), definition());
        let result = tool
            .execute(
                &invocation(
                    json!({"path":"bad.txt","old_string":"a","new_string":"x","replace_all":true}),
                ),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert!(result.text.starts_with("error: "));
        assert_eq!(service.0.lock().unwrap().mutation_bytes, Some(bytes));
        assert_eq!(service.calls().len(), 1);
    }
}

#[tokio::test]
async fn cancelled_mutations_have_no_service_dispatch_or_effects() {
    let service = RecordingService::default();
    service.0.lock().unwrap().mutation_bytes = Some(b"old".to_vec());
    let write = WriteTool::new(service.clone(), WorkspacePaths::default(), definition());
    let edit = EditTool::new(service.clone(), WorkspacePaths::default(), definition());
    assert_eq!(
        write
            .execute(
                &invocation(json!({"path":"file","content":"new"})),
                &Snapshot(1),
                &AlreadyCancelled
            )
            .await,
        Err(OperationError::Cancelled)
    );
    assert_eq!(
        edit.execute(
            &invocation(json!({"path":"file","old_string":"old","new_string":"new"})),
            &Snapshot(1),
            &AlreadyCancelled
        )
        .await,
        Err(OperationError::Cancelled)
    );
    assert!(service.calls().is_empty());
    assert_eq!(
        service.0.lock().unwrap().mutation_bytes.as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        write.parallel_safety(&invocation(json!({}))),
        ParallelSafety::Sequential
    );
    assert_eq!(
        edit.parallel_safety(&invocation(json!({}))),
        ParallelSafety::Sequential
    );
}

#[tokio::test]
async fn glob_recurses_real_directories_reports_symlink_directories_without_following_and_sorts() {
    let service = RecordingService::default();
    service.directory(
        "root",
        vec![
            metadata("root/z.txt", "root/z.txt", EntryKind::Regular),
            metadata("root/sub", "root/sub", EntryKind::Directory),
            metadata("root/link", "/external", EntryKind::SymbolicLink),
        ],
    );
    service.directory(
        "root/sub",
        vec![metadata(
            "root/sub/a.txt",
            "root/sub/a.txt",
            EntryKind::Regular,
        )],
    );
    service.0.lock().unwrap().metadata.insert(
        "root/link".into(),
        metadata("root/link", "/external", EntryKind::Directory),
    );
    let tool = GlobTool::new(service.clone(), definition());
    let result = tool
        .invoke(
            invocation(json!({"path":"root","pattern":"**"})),
            InvocationContext::new(Snapshot(47), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "link/\nsub/\nsub/a.txt\nz.txt\n");
    let calls = service.calls();
    assert_shared_snapshot(&calls, 47);
    let lists = calls
        .iter()
        .filter_map(|call| match &call.action {
            Action::List(path, links) => Some((path.to_string_lossy().into_owned(), *links)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lists,
        vec![
            ("root".into(), LinkPolicy::Follow),
            ("root/sub".into(), LinkPolicy::Follow)
        ]
    );
    let recursive = tool
        .execute(
            &invocation(json!({"path":"root","pattern":"**/*.txt"})),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(recursive.text, "sub/a.txt\nz.txt\n");
    let top = tool
        .execute(
            &invocation(json!({"path":"root","pattern":"*.txt"})),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(top.text, "z.txt\n");
}

#[tokio::test]
async fn glob_distinguishes_completed_exact_limit_from_truncated_extra_result() {
    for count in [1000, 1001] {
        let service = RecordingService::default();
        service.directory(
            ".",
            (0..count)
                .map(|index| {
                    let path = format!("./file-{index:04}.txt");
                    metadata(&path, &path, EntryKind::Regular)
                })
                .collect(),
        );
        let tool = GlobTool::new(service, definition());
        let result = tool
            .execute(
                &invocation(json!({"pattern":"*.txt"})),
                &Snapshot(1),
                &NoCancel,
            )
            .await
            .unwrap();
        assert_eq!(
            result
                .text
                .lines()
                .filter(|line| line.ends_with(".txt"))
                .count(),
            1000
        );
        assert_eq!(
            result.text.contains("[glob results truncated:"),
            count == 1001
        );
        if count == 1001 {
            assert!(
                result
                    .text
                    .ends_with("[glob results truncated: result limit reached]\n")
            );
        }
    }
}

#[tokio::test]
async fn glob_caps_visited_entries_even_when_no_pattern_matches() {
    let service = RecordingService::default();
    service.directory(
        ".",
        (0..100_001)
            .map(|index| {
                let path = format!("./file-{index:06}");
                metadata(&path, &path, EntryKind::Regular)
            })
            .collect(),
    );
    let tool = GlobTool::new(service.clone(), definition());
    let result = tool
        .execute(
            &invocation(json!({"pattern":"*.never"})),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(
        result.text,
        "[glob results truncated: visit limit reached]\n"
    );
    assert_eq!(
        service
            .calls()
            .iter()
            .filter(|call| matches!(call.action, Action::Inspect(_, _)))
            .count(),
        100_001
    );
}

#[tokio::test]
async fn glob_deadline_is_checked_even_when_the_service_never_yields() {
    let service = RecordingService::default();
    service.directory(
        ".",
        vec![metadata("./a.txt", "./a.txt", EntryKind::Regular)],
    );
    service.0.lock().unwrap().blocking_listing = Some(std::time::Duration::from_millis(5100));
    let tool = GlobTool::new(service.clone(), definition());
    let result = tool
        .execute(&invocation(json!({"pattern":"*"})), &Snapshot(1), &NoCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "error: glob timed out");
    assert_eq!(
        service
            .calls()
            .iter()
            .filter(|call| matches!(call.action, Action::Inspect(_, _)))
            .count(),
        1
    );
}

#[tokio::test]
async fn glob_propagates_cancellation_and_uses_real_tokio_timeout() {
    let service = RecordingService::default();
    service.directory(".", vec![]);
    let tool = GlobTool::new(service.clone(), definition());
    assert_eq!(
        tool.execute(
            &invocation(json!({"pattern":"*"})),
            &Snapshot(1),
            &AlreadyCancelled
        )
        .await,
        Err(OperationError::Cancelled)
    );
    service.0.lock().unwrap().pending_listing = true;
    let result = tool
        .execute(&invocation(json!({"pattern":"*"})), &Snapshot(1), &NoCancel)
        .await
        .unwrap();
    assert_eq!(result.text, "error: glob timed out");
}
