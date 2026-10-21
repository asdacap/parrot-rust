use super::*;
#[path = "agents_decoding.rs"]
mod decoding;
use parrot_domain::contracts::{
    async_ops::{OperationError, OperationResult},
    filesystem::{
        AuthorizedReadService, BoundRead, EntryKind, FileMetadata, FileReadError,
        FileReadErrorKind, LinkPolicy, ReadRequest,
    },
    paths::ResolvedRead,
    security::{AccessSnapshot, SecurityProvider},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

struct Snapshot(usize);
impl AccessSnapshot for Snapshot {
    type Error = FileReadError;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        panic!("prompt must not request writes")
    }
}
struct Policy(AtomicUsize);
impl SecurityProvider for Policy {
    type Snapshot = Snapshot;
    fn capture(&self) -> Snapshot {
        Snapshot(self.0.fetch_add(1, Ordering::SeqCst))
    }
}
#[derive(Default)]
struct Reader {
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    calls: Mutex<Vec<(String, usize)>>,
    denied: Mutex<Option<PathBuf>>,
}
fn failure(kind: FileReadErrorKind, message: &str) -> OperationError<FileReadError> {
    OperationError::Failed(FileReadError::new(kind, message))
}
impl AuthorizedReadService for Reader {
    type Security = Snapshot;
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            assert_eq!(links, LinkPolicy::Follow);
            self.calls
                .lock()
                .unwrap()
                .push((format!("inspect:{}", path.display()), snapshot.0));
            if self.denied.lock().unwrap().as_deref() == Some(path) {
                return Err(failure(FileReadErrorKind::Denied, "denied actual target"));
            }
            if !self.files.lock().unwrap().contains_key(path) {
                return Err(failure(FileReadErrorKind::NotFound, "absent"));
            }
            Ok(FileMetadata {
                lexical_path: path.into(),
                physical_path: path.into(),
                kind: EntryKind::Regular,
                hidden: false,
            })
        })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("AGENTS sampling reads only the exact ancestry filenames")
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            assert_eq!(request.links, LinkPolicy::Follow);
            assert_eq!(
                request.expected_physical.as_deref(),
                Some(request.requested.as_path())
            );
            assert_eq!(request.max_bytes, None);
            self.calls
                .lock()
                .unwrap()
                .push((format!("read:{}", request.requested.display()), snapshot.0));
            let bytes = self
                .files
                .lock()
                .unwrap()
                .get(&request.requested)
                .cloned()
                .ok_or_else(|| {
                    failure(FileReadErrorKind::TargetChanged, "opened target changed")
                })?;
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: request.requested.clone(),
                bytes,
            })
        })
    }
}
struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
fn fixture() -> (Arc<Reader>, Arc<Policy>) {
    let reader = Arc::new(Reader::default());
    for (path, content) in [
        ("/work/sub/AGENTS.md", "child"),
        ("/work/AGENTS.md", "parent"),
        ("/AGENTS.md", "root"),
        ("/config/AGENTS.md", "global"),
    ] {
        reader
            .files
            .lock()
            .unwrap()
            .insert(path.into(), content.as_bytes().to_vec());
    }
    (reader, Arc::new(Policy(AtomicUsize::new(0))))
}

#[tokio::test]
async fn agents_global_then_ancestry_order_and_epoch_resampling_are_actual_reads() {
    let (reader, policy) = fixture();
    let provider = AgentsPromptProvider::new(
        "/work/sub",
        "/config",
        reader.clone(),
        policy.clone(),
        templates(""),
    )
    .unwrap();
    let mut prompt = provider.materialize(&identity()).unwrap();
    let (_, router) = route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert_eq!(
        prompt.build(&selection).unwrap_err().to_string(),
        "The system context has not been sampled for this epoch."
    );
    prompt.sample_epoch(&Never).await.unwrap();
    let first = prompt.build(&selection).unwrap();
    let positions: Vec<_> = ["global", "root", "parent", "child"]
        .iter()
        .map(|s| first.find(s).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        reader
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, epoch)| *epoch == 0)
    );
    let count = reader.calls.lock().unwrap().len();
    reader
        .files
        .lock()
        .unwrap()
        .insert("/work/sub/AGENTS.md".into(), b"changed child".to_vec());
    assert_eq!(prompt.build(&selection).unwrap(), first);
    assert_eq!(reader.calls.lock().unwrap().len(), count);
    prompt.sample_epoch(&Never).await.unwrap();
    assert!(prompt.build(&selection).unwrap().contains("changed child"));
    assert_eq!(policy.0.load(Ordering::SeqCst), 2);
    assert!(
        reader.calls.lock().unwrap()[count..]
            .iter()
            .all(|(_, epoch)| *epoch == 1)
    );
}

#[tokio::test]
async fn global_already_in_ancestry_is_not_duplicated_and_failures_do_not_become_missing() {
    let (reader, policy) = fixture();
    let provider =
        AgentsPromptProvider::new("/work/sub", "/work", reader.clone(), policy, templates(""))
            .unwrap();
    let mut prompt = provider.materialize(&identity()).unwrap();
    prompt.sample_epoch(&Never).await.unwrap();
    assert_eq!(
        reader
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(call, _)| call == "read:/work/AGENTS.md")
            .count(),
        1
    );
    *reader.denied.lock().unwrap() = Some("/work/sub/AGENTS.md".into());
    assert_eq!(
        prompt.sample_epoch(&Never).await.unwrap_err().to_string(),
        "denied actual target"
    );
    // The source commits a sampled epoch only after its complete successful scan.
    let (_, router) = route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert!(prompt.build(&selection).unwrap().contains("child"));
    prompt.renew_epoch();
    assert!(prompt.build(&selection).is_err());
    let before = reader.calls.lock().unwrap().len();
    assert_eq!(
        prompt.sample_epoch(&Cancelled).await.unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(reader.calls.lock().unwrap().len(), before);
}

struct SamplingPrompt {
    id: usize,
    visits: Arc<Mutex<Vec<usize>>>,
    fail: bool,
}
impl SystemPrompt for SamplingPrompt {
    fn renew_epoch(&mut self) {
        panic!("async composite must call the child sampling operation")
    }
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async move {
            self.visits.lock().unwrap().push(self.id);
            if self.fail {
                Err(PromptError::Context("sample failure".into()))
            } else {
                Ok(())
            }
        })
    }
    fn build(&mut self, _: &ResolvedModelSelection) -> PromptResult<String> {
        Ok(self.id.to_string())
    }
}
#[tokio::test]
async fn composite_sampling_is_ordered_and_stops_at_the_failing_child_without_rollback_claim() {
    let visits = Arc::new(Mutex::new(Vec::new()));
    let mut composite = CompositeSystemPrompt::new(
        (0..3)
            .map(|id| {
                Box::new(SamplingPrompt {
                    id,
                    visits: visits.clone(),
                    fail: id == 1,
                }) as Box<dyn SystemPrompt>
            })
            .collect(),
    );
    assert!(composite.sample_epoch(&Never).await.is_err());
    assert_eq!(*visits.lock().unwrap(), [0, 1]);
}
