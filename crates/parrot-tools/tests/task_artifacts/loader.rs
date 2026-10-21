use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use parrot_domain::contracts::{
    agent_tasks::TaskArtifactParser,
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    filesystem::{
        AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
        LinkPolicy, ReadRequest,
    },
    paths::ResolvedRead,
    security::AccessSnapshot,
};
use parrot_tools::task_artifacts::{
    ArtifactReadError, BeforeToolBatch, TaskApplicationError, TaskArtifactParserAdapter,
    TaskInputError, TaskInputInvocation, apply_task_input, parse_task_input, read_artifact,
};

#[derive(Debug)]
struct SnapshotError;
impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("fixture snapshot error")
    }
}
impl std::error::Error for SnapshotError {}

struct Snapshot;
impl AccessSnapshot for Snapshot {
    type Error = SnapshotError;

    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        Err(SnapshotError)
    }

    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Err(SnapshotError)
    }
}

struct NoCancel;
impl Cancellation for NoCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

struct FixtureReader {
    bytes: Vec<u8>,
    failure: Option<OperationError<FileReadError>>,
    request: Mutex<Option<ReadRequest>>,
}

impl FixtureReader {
    fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            failure: None,
            request: Mutex::new(None),
        }
    }

    fn failure(error: OperationError<FileReadError>) -> Self {
        Self {
            bytes: Vec::new(),
            failure: Some(error),
            request: Mutex::new(None),
        }
    }
}

impl AuthorizedReadService for FixtureReader {
    type Security = Snapshot;

    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Self::Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async {
            Err(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::InvalidOperation,
                "fixture reader does not inspect",
            )))
        })
    }

    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Self::Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async {
            Err(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::InvalidOperation,
                "fixture reader does not list",
            )))
        })
    }

    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        _: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        *self.request.lock().unwrap() = Some(request.clone());
        let bytes = self.bytes.clone();
        let failure = self.failure.clone();
        let requested = request.requested.clone();
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if let Some(failure) = failure {
                return Err(failure);
            }
            Ok(BoundRead {
                lexical_path: requested.clone(),
                physical_path: requested,
                bytes,
            })
        })
    }
}

fn artifact_bytes() -> Vec<u8> {
    br#"{"schema_version":1,"tasks":[{"name":"a","description":"d","payload":"p","acceptance_criteria":"c"}]}"#.to_vec()
}

fn task_set_json() -> &'static str {
    r#"[{"name":"a","description":"d","payload":"p","acceptance_criteria":"c"}]"#
}

#[tokio::test]
async fn loader_requests_unbounded_regular_read_with_reject_links_and_parses_after_read() {
    let reader = FixtureReader::bytes([b"\xEF\xBB\xBF".as_slice(), &artifact_bytes()].concat());
    let requested = PathBuf::from("/selected/tasks.json");
    let parsed = read_artifact(&reader, &requested, &Snapshot, &NoCancel)
        .await
        .unwrap();

    assert_eq!(parsed.tasks.len(), 1);
    assert_eq!(parsed.tasks[0].name, "a");
    assert_eq!(
        reader.request.lock().unwrap().as_ref().unwrap(),
        &ReadRequest {
            requested,
            expected_physical: None,
            links: LinkPolicy::RejectLinks,
            max_bytes: None,
        }
    );
}

#[tokio::test]
async fn loader_preserves_typed_read_failure_and_cancellation() {
    let denied = FileReadError::new(FileReadErrorKind::Denied, "source denial");
    let result = read_artifact(
        &FixtureReader::failure(OperationError::Failed(denied.clone())),
        Path::new("tasks.json"),
        &Snapshot,
        &NoCancel,
    )
    .await;
    assert_eq!(
        result,
        Err(OperationError::Failed(ArtifactReadError::Read(denied)))
    );

    let cancelled = read_artifact(
        &FixtureReader::failure(OperationError::Cancelled),
        Path::new("tasks.json"),
        &Snapshot,
        &NoCancel,
    )
    .await;
    assert_eq!(cancelled, Err(OperationError::Cancelled));
}

#[tokio::test]
async fn loader_reports_parse_error_without_erasing_read_boundary() {
    let result = read_artifact(
        &FixtureReader::bytes(b"{}".to_vec()),
        Path::new("tasks.json"),
        &Snapshot,
        &NoCancel,
    )
    .await;

    assert!(matches!(
        result,
        Err(OperationError::Failed(ArtifactReadError::Parse(error)))
            if error.to_string() == "schema_version is required."
    ));
}

#[tokio::test]
async fn task_input_requires_exactly_one_source_before_read_or_parse() {
    for (path_present, path, tasks_present, tasks_json) in [
        (false, None, false, ""),
        (true, Some("tasks.json"), true, "[]"),
    ] {
        let reader = FixtureReader::bytes(artifact_bytes());
        let result = parse_task_input(
            path_present,
            path,
            tasks_present,
            tasks_json,
            &reader,
            &Snapshot,
            &NoCancel,
        )
        .await;
        assert!(matches!(
            result,
            Err(OperationError::Failed(TaskInputError::Arguments(
                "Tool arguments require exactly one of a nonblank string 'path' or a 'tasks' array."
            )))
        ));
        assert!(reader.request.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn task_input_selects_valid_inline_and_file_sources_without_dispatching() {
    let inline_reader = FixtureReader::bytes(Vec::new());
    let inline = parse_task_input(
        false,
        None,
        true,
        task_set_json(),
        &inline_reader,
        &Snapshot,
        &NoCancel,
    )
    .await
    .unwrap();
    assert_eq!(inline.len(), 1);
    assert!(inline_reader.request.lock().unwrap().is_none());

    let file_reader = FixtureReader::bytes(artifact_bytes());
    let file = parse_task_input(
        true,
        Some("tasks.json"),
        false,
        "",
        &file_reader,
        &Snapshot,
        &NoCancel,
    )
    .await
    .unwrap();
    assert_eq!(file.len(), 1);
    assert_eq!(
        file_reader.request.lock().unwrap().as_ref().unwrap().links,
        LinkPolicy::RejectLinks
    );
}

#[tokio::test]
async fn task_input_requires_a_nonblank_path_and_preserves_inline_parse_errors() {
    let reader = FixtureReader::bytes(artifact_bytes());
    let blank_path =
        parse_task_input(true, Some("  "), false, "", &reader, &Snapshot, &NoCancel).await;
    assert!(matches!(
        blank_path,
        Err(OperationError::Failed(TaskInputError::Arguments(
            "Tool arguments require a nonblank string 'path'."
        )))
    ));
    assert!(reader.request.lock().unwrap().is_none());

    let inline_error =
        parse_task_input(false, None, true, "[]", &reader, &Snapshot, &NoCancel).await;
    assert!(matches!(
        inline_error,
        Err(OperationError::Failed(TaskInputError::Inline(error)))
            if error.to_string() == "tasks must not be empty."
    ));
    assert!(reader.request.lock().unwrap().is_none());
}

#[derive(Debug, Eq, PartialEq)]
struct DispatchFailure(&'static str);
impl std::fmt::Display for DispatchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for DispatchFailure {}

#[tokio::test]
async fn application_dispatches_only_parsed_tasks_with_same_selection_and_boundary() {
    let reader = FixtureReader::bytes(artifact_bytes());
    let selection = String::from("immutable selection");
    let selection_address = &selection as *const String;
    let accepted = apply_task_input(
        &reader,
        TaskInputInvocation {
            path_present: false,
            path: None,
            tasks_present: true,
            tasks_json: task_set_json(),
            snapshot: &Snapshot,
            cancellation: &NoCancel,
            selection: &selection,
            assistant_sequence: 17,
            call_id: "call-17",
        },
        move |tasks, actual_selection, boundary| {
            assert_eq!(tasks.len(), 1);
            assert!(std::ptr::eq(
                selection_address,
                actual_selection as *const String
            ));
            assert_eq!(actual_selection, "immutable selection");
            assert_eq!(
                boundary,
                BeforeToolBatch {
                    assistant_sequence: 17,
                    call_id: "call-17".into(),
                }
            );
            Ok::<_, DispatchFailure>(())
        },
    )
    .await
    .unwrap();

    assert_eq!(accepted, 1);
}

#[tokio::test]
async fn application_never_dispatches_rejected_inputs_and_does_not_accept_dispatch_errors() {
    let not_dispatched = std::sync::atomic::AtomicBool::new(false);
    let bad_input = apply_task_input(
        &FixtureReader::bytes(Vec::new()),
        TaskInputInvocation {
            path_present: false,
            path: None,
            tasks_present: true,
            tasks_json: "[]",
            snapshot: &Snapshot,
            cancellation: &NoCancel,
            selection: &(),
            assistant_sequence: 1,
            call_id: "rejected",
        },
        |_, _, _| {
            not_dispatched.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, DispatchFailure>(())
        },
    )
    .await;
    assert!(matches!(
        bad_input,
        Err(OperationError::Failed(TaskApplicationError::Input(
            TaskInputError::Inline(_)
        )))
    ));
    assert!(!not_dispatched.load(std::sync::atomic::Ordering::SeqCst));

    let read_rejected = apply_task_input(
        &FixtureReader::failure(OperationError::Failed(FileReadError::new(
            FileReadErrorKind::Denied,
            "source denial",
        ))),
        TaskInputInvocation {
            path_present: true,
            path: Some("tasks.json"),
            tasks_present: false,
            tasks_json: "",
            snapshot: &Snapshot,
            cancellation: &NoCancel,
            selection: &(),
            assistant_sequence: 1,
            call_id: "read-rejected",
        },
        |_, _, _| {
            not_dispatched.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, DispatchFailure>(())
        },
    )
    .await;
    assert!(matches!(
        read_rejected,
        Err(OperationError::Failed(TaskApplicationError::Input(
            TaskInputError::Artifact(ArtifactReadError::Read(_))
        )))
    ));
    assert!(!not_dispatched.load(std::sync::atomic::Ordering::SeqCst));

    let read_cancelled = apply_task_input(
        &FixtureReader::failure(OperationError::Cancelled),
        TaskInputInvocation {
            path_present: true,
            path: Some("tasks.json"),
            tasks_present: false,
            tasks_json: "",
            snapshot: &Snapshot,
            cancellation: &NoCancel,
            selection: &(),
            assistant_sequence: 1,
            call_id: "read-cancelled",
        },
        |_, _, _| {
            not_dispatched.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, DispatchFailure>(())
        },
    )
    .await;
    assert_eq!(read_cancelled, Err(OperationError::Cancelled));
    assert!(!not_dispatched.load(std::sync::atomic::Ordering::SeqCst));

    let dispatch_failure = apply_task_input(
        &FixtureReader::bytes(Vec::new()),
        TaskInputInvocation {
            path_present: false,
            path: None,
            tasks_present: true,
            tasks_json: task_set_json(),
            snapshot: &Snapshot,
            cancellation: &NoCancel,
            selection: &(),
            assistant_sequence: 2,
            call_id: "failed-dispatch",
        },
        |_, _, _| Err::<(), _>(DispatchFailure("task graph rejected")),
    )
    .await;
    assert_eq!(
        dispatch_failure,
        Err(OperationError::Failed(TaskApplicationError::Dispatch(
            DispatchFailure("task graph rejected")
        )))
    );
}

#[test]
fn tools_adapter_implements_the_domain_parser_contract_without_a_second_parser() {
    let parser = TaskArtifactParserAdapter;
    let shared: &dyn TaskArtifactParser<Error = parrot_tools::task_artifacts::ParseError> = &parser;
    assert_eq!(shared.parse_artifact(r#"{"schema_version":1,"tasks":[{"name":"a","description":"d","payload":"p","acceptance_criteria":"c"}]}"#).unwrap().tasks[0].name, "a");
    assert_eq!(
        shared
            .parse_task_set(
                r#"[{"name":"a","description":"d","payload":"p","acceptance_criteria":"c"}]"#
            )
            .unwrap()
            .len(),
        1
    );
    assert!(shared.validate_merged_graph(&[], "tasks").is_ok());
    assert!(
        shared
            .parse_leaf_response(r#"{"result":"ok","verdict":"accept"}"#)
            .is_ok()
    );
}
