//! Synthetic fixture-reader tests prove Tool orchestration, not a production
//! filesystem authorization implementation or the cancelled T36 approval path.

use super::runtime_fixture::{Fixture, Security, context, invocation, json};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    filesystem::{
        AuthorizedReadService, BoundRead, EntryKind, FileMetadata, FileReadError,
        FileReadErrorKind, LinkPolicy, ReadRequest,
    },
    tools::Tool,
};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::events::NeverCancelled;
use parrot_tools::tasks_queues::queues::{
    MAXIMUM_SOURCE_FILE_BYTES,
    QueueToolKind::{Create, Push},
};
use serde_json::json;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct FixtureReader {
    root: PathBuf,
    denied: Vec<PathBuf>,
    failure: Option<FileReadErrorKind>,
    requests: Mutex<Vec<ReadRequest>>,
}

impl FixtureReader {
    fn new(denied_leaf: Option<&str>, failure: Option<FileReadErrorKind>) -> Arc<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp/t59-source-file-fixtures")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
        fs::create_dir_all(&root).unwrap();
        Arc::new(Self {
            denied: denied_leaf
                .map(|leaf| root.join(leaf))
                .into_iter()
                .collect(),
            root,
            failure,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn resolve(&self, path: &Path) -> Result<(PathBuf, PathBuf), FileReadError> {
        let lexical = if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        };
        if self.denied.iter().any(|denied| lexical.starts_with(denied)) {
            return Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied lexical path",
            ));
        }
        let physical = fs::canonicalize(&lexical).map_err(read_error)?;
        if !physical.starts_with(&self.root)
            || self
                .denied
                .iter()
                .any(|denied| physical.starts_with(denied))
        {
            return Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied physical path",
            ));
        }
        Ok((lexical, physical))
    }
}

fn read_error(error: std::io::Error) -> FileReadError {
    FileReadError::new(
        match error.kind() {
            std::io::ErrorKind::NotFound => FileReadErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => FileReadErrorKind::Denied,
            _ => FileReadErrorKind::Io,
        },
        error.to_string(),
    )
}

impl AuthorizedReadService for FixtureReader {
    type Security = Security;
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        _: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            assert_eq!(links, LinkPolicy::Follow);
            let (lexical_path, physical_path) =
                self.resolve(path).map_err(OperationError::Failed)?;
            let metadata = fs::metadata(&physical_path)
                .map_err(|error| OperationError::Failed(read_error(error)))?;
            Ok(FileMetadata {
                lexical_path,
                physical_path,
                kind: if metadata.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::Regular
                },
                hidden: false,
            })
        })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async { panic!("queue source loading must not enumerate directories") })
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        _: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.requests.lock().unwrap().push(request.clone());
            assert_eq!(request.links, LinkPolicy::Follow);
            assert_eq!(request.max_bytes, Some(MAXIMUM_SOURCE_FILE_BYTES));
            let (lexical_path, physical_path) = self
                .resolve(&request.requested)
                .map_err(OperationError::Failed)?;
            assert_eq!(
                request.expected_physical.as_deref(),
                Some(physical_path.as_path())
            );
            if let Some(kind) = self.failure {
                return Err(OperationError::Failed(FileReadError::new(
                    kind,
                    "injected fixture read failure",
                )));
            }
            let mut file = fs::File::open(&physical_path)
                .map_err(|error| OperationError::Failed(read_error(error)))?;
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                cancellation
                    .check()
                    .map_err(|_| OperationError::Cancelled)?;
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| OperationError::Failed(read_error(error)))?;
                if count == 0 {
                    break;
                }
                if bytes.len() + count > request.max_bytes.unwrap() {
                    return Err(OperationError::Failed(FileReadError::new(
                        FileReadErrorKind::TooLarge,
                        "fixture actual byte limit",
                    )));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Ok(BoundRead {
                lexical_path,
                physical_path,
                bytes,
            })
        })
    }
}

async fn prepare() -> Fixture {
    let f = Fixture::new().await;
    f.call(&f.root, Create, r#"{"name":"work"}"#).await;
    f
}

async fn unchanged_after_error(reader: Arc<FixtureReader>, source: &str, expected: &str) {
    let f = prepare().await;
    f.call(&f.root, Push, r#"{"name":"work","items":["existing"]}"#)
        .await;
    let arguments =
        json!({"name":"work","source_file":source,"direction":"back","close":true}).to_string();
    let output = f
        .tool_with_reader(&f.root, Push, reader)
        .invoke(invocation("file", &arguments), context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(output.text, expected);
    let q = f.runtime.q(&f.root);
    let taken = q
        .try_take("work", 10, parrot_storage::queue_files::Direction::Front)
        .unwrap();
    assert_eq!(taken.items, ["existing"]);
    assert!(!taken.info.unwrap().closed);
    f.close().await;
}

// Original QueuePushSourceFileToolTests.Loads_nonblank_UTF8_lines_and_preserves_retained_text@23.
#[tokio::test]
async fn source_file_loads_strict_bom_lines_before_real_queue_mutation() {
    let reader = FixtureReader::new(None, None);
    fs::write(
        reader.root.join("items.txt"),
        "\u{feff}first\r\n\r\n  \r\n second \nthird\n",
    )
    .unwrap();
    let f = prepare().await;
    let output = f
        .tool_with_reader(&f.root, Push, reader.clone())
        .invoke(
            invocation(
                "file",
                r#"{"name":"work","source_file":"items.txt","direction":"back"}"#,
            ),
            context(&NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(json(output)["size"], 3);
    assert_eq!(
        f.runtime
            .q(&f.root)
            .try_take("work", 10, parrot_storage::queue_files::Direction::Front)
            .unwrap()
            .items,
        ["first", " second ", "third"]
    );
    assert_eq!(reader.requests.lock().unwrap().len(), 1);
    f.close().await;
}

// Original QueuePushSourceFileToolTests.Loads_authorized_absolute_files_and_applies_front_direction_and_close@47.
#[tokio::test]
async fn fixture_authorized_absolute_source_applies_front_and_close() {
    let reader = FixtureReader::new(None, None);
    let external = reader.root.join("external");
    fs::create_dir(&external).unwrap();
    let source = external.join("items.txt");
    fs::write(&source, "one\ntwo\n").unwrap();
    let f = prepare().await;
    let args =
        json!({"name":"work","source_file":source,"direction":"front","close":true}).to_string();
    let output = f
        .tool_with_reader(&f.root, Push, reader)
        .invoke(invocation("file", &args), context(&NeverCancelled))
        .await
        .unwrap();
    assert_eq!(json(output)["closed"], true);
    let taken = f
        .runtime
        .q(&f.root)
        .try_take("work", 10, parrot_storage::queue_files::Direction::Front)
        .unwrap();
    assert_eq!(taken.items, ["two", "one"]);
    assert!(taken.info.unwrap().closed);
    f.close().await;
}

// Original QueuePushSourceFileToolTests.Empty_or_whitespace_only_source_matches_an_empty_inline_push@72.
#[tokio::test]
async fn whitespace_source_can_close_and_promptly_drain_real_queue() {
    let reader = FixtureReader::new(None, None);
    fs::write(reader.root.join("empty.txt"), " \n\t\r\n").unwrap();
    let f = prepare().await;
    let output = f
        .tool_with_reader(&f.root, Push, reader)
        .invoke(
            invocation(
                "file",
                r#"{"name":"work","source_file":"empty.txt","close":true}"#,
            ),
            context(&NeverCancelled),
        )
        .await
        .unwrap();
    let value = json(output);
    assert_eq!(value["size"], 0);
    assert_eq!(value["closed"], true);
    let taken = f
        .runtime
        .q(&f.root)
        .try_take("work", 1, parrot_storage::queue_files::Direction::Front)
        .unwrap();
    assert!(taken.acquired && taken.items.is_empty());
    f.close().await;
}

#[tokio::test]
async fn denied_fixture_direct_path_preserves_existing_items_and_open_state() {
    let reader = FixtureReader::new(Some("denied.txt"), None);
    fs::write(reader.root.join("denied.txt"), "secret\n").unwrap();
    unchanged_after_error(reader, "denied.txt", "error: access denied").await;
}

#[cfg(unix)]
#[tokio::test]
async fn denied_fixture_symlink_alias_and_target_preserve_real_queue() {
    for denied in ["alias", "target"] {
        let reader = FixtureReader::new(Some(denied), None);
        fs::create_dir(reader.root.join("target")).unwrap();
        fs::write(reader.root.join("target/items.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(reader.root.join("target"), reader.root.join("alias")).unwrap();
        unchanged_after_error(reader, "alias/items.txt", "error: access denied").await;
    }
}

#[tokio::test]
async fn missing_directory_binary_and_invalid_utf8_never_mutate_or_close() {
    let reader = FixtureReader::new(None, None);
    fs::create_dir(reader.root.join("directory")).unwrap();
    fs::write(reader.root.join("binary.txt"), [0x61, 0, 0x62]).unwrap();
    fs::write(reader.root.join("invalid.txt"), [0x61, 0xc3, 0x28]).unwrap();
    for (source, expected) in [
        ("missing.txt", "error: no such file or directory"),
        ("directory", "error: source_file must be a file"),
        ("binary.txt", "error: binary file"),
        ("invalid.txt", "error: source_file is not valid UTF-8"),
    ] {
        unchanged_after_error(reader.clone(), source, expected).await;
    }
}

#[tokio::test]
async fn actual_source_byte_limit_and_distinct_final_queue_limit_preserve_state() {
    let reader = FixtureReader::new(None, None);
    fs::File::create(reader.root.join("large.txt"))
        .unwrap()
        .set_len((MAXIMUM_SOURCE_FILE_BYTES + 1) as u64)
        .unwrap();
    fs::write(reader.root.join("escaped.txt"), "\\".repeat(9 << 20)).unwrap();
    unchanged_after_error(
        reader.clone(),
        "large.txt",
        "error: source_file exceeds 16777216 bytes",
    )
    .await;
    unchanged_after_error(
        reader,
        "escaped.txt",
        "error: queue: file exceeds 16777216 bytes",
    )
    .await;
}

#[tokio::test]
async fn read_failure_after_inspection_preserves_existing_items_and_open_state() {
    let reader = FixtureReader::new(None, Some(FileReadErrorKind::Io));
    fs::write(reader.root.join("items.txt"), "item\n").unwrap();
    unchanged_after_error(
        reader.clone(),
        "items.txt",
        "error: could not read source_file",
    )
    .await;
    assert_eq!(reader.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn source_file_cancellation_propagates_without_queue_close_or_mutation() {
    let reader = FixtureReader::new(None, None);
    fs::write(reader.root.join("items.txt"), "item\n").unwrap();
    let f = prepare().await;
    let cancel = Signal::new();
    cancel.cancel();
    let output = f
        .tool_with_reader(&f.root, Push, reader)
        .invoke(
            invocation(
                "file",
                r#"{"name":"work","source_file":"items.txt","close":true}"#,
            ),
            context(&cancel),
        )
        .await;
    assert!(matches!(output, Err(OperationError::Cancelled)));
    let state = f.runtime.q(&f.root).get("work").unwrap();
    assert_eq!(state.size, 0);
    assert!(!state.closed);
    f.close().await;
}
