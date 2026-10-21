use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    filesystem::{
        AuthorizedReadService, BoundRead, EntryKind, FileMetadata, FileReadError, LinkPolicy,
        ReadConsumer, ReadRequest,
    },
};
use parrot_tools::files::{ReadTool, WorkspacePaths};
use serde_json::json;

use super::fixtures::{NoCancel, Snapshot, definition, invocation, metadata};

#[derive(Clone)]
struct GeneratedReader {
    chunk: Vec<u8>,
    repeats: usize,
    consumed: Arc<AtomicUsize>,
}

impl AuthorizedReadService for GeneratedReader {
    type Security = Snapshot;

    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async {
            Ok(metadata(
                "generated",
                "/actual/generated.txt",
                EntryKind::Regular,
            ))
        })
    }

    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("not a directory")
    }

    fn read<'a>(
        &'a self,
        _: &'a ReadRequest,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        panic!("Read tool must not use the buffered compatibility adapter")
    }

    fn stream_read<'a>(
        &'a self,
        request: &'a ReadRequest,
        _: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
        consumer: &'a mut dyn ReadConsumer,
    ) -> BoxFuture<'a, OperationResult<PathBuf, FileReadError>> {
        Box::pin(async move {
            assert_eq!(
                request.expected_physical.as_deref(),
                Some(Path::new("/actual/generated.txt"))
            );
            assert_eq!(request.max_bytes, None);
            assert!(self.chunk.len() <= 8192);
            for _ in 0..self.repeats {
                cancellation
                    .check()
                    .map_err(|_| OperationError::Cancelled)?;
                self.consumed.fetch_add(self.chunk.len(), Ordering::Relaxed);
                if !consumer.consume(&self.chunk) {
                    break;
                }
            }
            Ok("/actual/generated.txt".into())
        })
    }
}

#[tokio::test]
async fn early_binary_probe_stops_large_generated_input_after_one_chunk() {
    let reader = GeneratedReader {
        chunk: b"x\0".repeat(4096),
        repeats: 1_000_000,
        consumed: Arc::new(AtomicUsize::new(0)),
    };
    let tool = ReadTool::new(reader.clone(), WorkspacePaths::default(), definition());
    let result = tool
        .execute(
            &invocation(json!({"path":"generated"})),
            &Snapshot(1),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "error: binary file");
    assert_eq!(reader.consumed.load(Ordering::Relaxed), 8192);
}

#[tokio::test]
async fn one_line_output_still_consumes_all_generated_short_lines_in_bounded_chunks() {
    let reader = GeneratedReader {
        chunk: b"line\n".repeat(1000),
        repeats: 2000,
        consumed: Arc::new(AtomicUsize::new(0)),
    };
    let tool = ReadTool::new(reader.clone(), WorkspacePaths::default(), definition());
    let result = tool
        .execute(
            &invocation(json!({"path":"generated", "limit":1})),
            &Snapshot(2),
            &NoCancel,
        )
        .await
        .unwrap();
    assert_eq!(result.text, "1: line\ntotal lines in file: 2000000\n");
    assert_eq!(reader.consumed.load(Ordering::Relaxed), 10_000_000);
}
