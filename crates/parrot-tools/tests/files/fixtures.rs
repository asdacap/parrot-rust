//! Recording services test caller obligations, not filesystem authorization backends.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, EntryKind, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
        },
        paths::ResolvedRead,
        security::AccessSnapshot,
    },
    values::tools::ConfiguredToolDefinition,
};
use parrot_tools::files::FileToolInvocation;

pub struct NoCancel;
impl Cancellation for NoCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

pub struct AlreadyCancelled;
impl Cancellation for AlreadyCancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub struct Snapshot(pub u64);
impl AccessSnapshot for Snapshot {
    type Error = String;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), String> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Inspect(PathBuf, LinkPolicy),
    List(PathBuf, LinkPolicy),
    Read(ReadRequest),
    MutationInspect(MutationRequest),
    MutationRead(MutationRequest),
    MutationWrite(MutationRequest, Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Call {
    pub action: Action,
    pub snapshot_id: u64,
    pub snapshot_address: usize,
}

#[derive(Default)]
pub struct State {
    pub metadata: BTreeMap<PathBuf, FileMetadata>,
    pub inspect_errors: BTreeMap<PathBuf, FileReadError>,
    pub directories: BTreeMap<PathBuf, Vec<FileMetadata>>,
    pub reads: BTreeMap<PathBuf, BoundRead>,
    pub calls: Vec<Call>,
    pub mutation_bytes: Option<Vec<u8>>,
    pub mutation_read_target: Option<MutationTarget>,
    pub mutation_write_target: Option<MutationTarget>,
    pub reject_read: Option<FileReadError>,
    pub reject_write: Option<FileReadError>,
    pub ignore_read_limit: bool,
    pub pending_listing: bool,
    pub blocking_listing: Option<std::time::Duration>,
}

#[derive(Clone, Default)]
pub struct RecordingService(pub Arc<Mutex<State>>);
impl RecordingService {
    pub fn file(&self, requested: &str, physical: &str, bytes: impl Into<Vec<u8>>) {
        let mut state = self.0.lock().unwrap();
        state.metadata.insert(
            requested.into(),
            metadata(requested, physical, EntryKind::Regular),
        );
        state.reads.insert(
            requested.into(),
            BoundRead {
                lexical_path: requested.into(),
                physical_path: physical.into(),
                bytes: bytes.into(),
            },
        );
    }

    pub fn directory(&self, requested: &str, entries: Vec<FileMetadata>) {
        let mut state = self.0.lock().unwrap();
        state.metadata.insert(
            requested.into(),
            metadata(requested, requested, EntryKind::Directory),
        );
        for entry in &entries {
            state
                .metadata
                .insert(entry.lexical_path.clone(), entry.clone());
        }
        state.directories.insert(requested.into(), entries);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }

    fn record(&self, action: Action, snapshot: &Snapshot) {
        self.0.lock().unwrap().calls.push(Call {
            action,
            snapshot_id: snapshot.0,
            snapshot_address: std::ptr::from_ref(snapshot) as usize,
        });
    }
}

pub fn metadata(lexical: &str, physical: &str, kind: EntryKind) -> FileMetadata {
    FileMetadata {
        lexical_path: lexical.into(),
        physical_path: physical.into(),
        kind,
        hidden: false,
    }
}

pub fn definition() -> ConfiguredToolDefinition {
    ConfiguredToolDefinition::new("fixture", "{}")
}

pub fn invocation(arguments: serde_json::Value) -> FileToolInvocation {
    FileToolInvocation::new("test-call", arguments.to_string())
}

pub fn assert_shared_snapshot(calls: &[Call], id: u64) {
    assert!(!calls.is_empty());
    for call in calls {
        assert_eq!(call.snapshot_id, id);
        assert_eq!(call.snapshot_address, calls[0].snapshot_address);
    }
}

fn check(cancellation: &dyn Cancellation) -> OperationResult<(), FileReadError> {
    cancellation.check().map_err(|_| OperationError::Cancelled)
}

fn missing() -> OperationError<FileReadError> {
    OperationError::Failed(FileReadError::new(
        FileReadErrorKind::NotFound,
        "fixture missing",
    ))
}

impl AuthorizedReadService for RecordingService {
    type Security = Snapshot;

    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            self.record(Action::Inspect(path.into(), links), snapshot);
            check(cancellation)?;
            let state = self.0.lock().unwrap();
            if let Some(error) = state.inspect_errors.get(path) {
                return Err(OperationError::Failed(error.clone()));
            }
            state.metadata.get(path).cloned().ok_or_else(missing)
        })
    }

    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async move {
            self.record(Action::List(path.into(), links), snapshot);
            check(cancellation)?;
            let pending = self.0.lock().unwrap().pending_listing;
            if pending {
                std::future::pending::<()>().await;
            }
            let blocking = self.0.lock().unwrap().blocking_listing;
            if let Some(duration) = blocking {
                std::thread::sleep(duration);
            }
            self.0
                .lock()
                .unwrap()
                .directories
                .get(path)
                .cloned()
                .ok_or_else(missing)
        })
    }

    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            self.record(Action::Read(request.clone()), snapshot);
            check(cancellation)?;
            let state = self.0.lock().unwrap();
            if let Some(error) = &state.reject_read {
                return Err(OperationError::Failed(error.clone()));
            }
            let read = state
                .reads
                .get(&request.requested)
                .cloned()
                .ok_or_else(missing)?;
            if !state.ignore_read_limit
                && request
                    .max_bytes
                    .is_some_and(|limit| read.bytes.len() > limit)
            {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TooLarge,
                    "fixture byte limit",
                )));
            }
            Ok(read)
        })
    }
}

impl AuthorizedMutationService for RecordingService {
    type Security = Snapshot;

    fn inspect_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async move {
            self.record(Action::MutationInspect(request.clone()), snapshot);
            check(cancellation)?;
            Ok(target(request))
        })
    }

    fn read_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>> {
        Box::pin(async move {
            self.record(Action::MutationRead(request.clone()), snapshot);
            check(cancellation)?;
            let state = self.0.lock().unwrap();
            if let Some(error) = &state.reject_read {
                return Err(OperationError::Failed(error.clone()));
            }
            Ok(MutationRead {
                target: state
                    .mutation_read_target
                    .clone()
                    .unwrap_or_else(|| target(request)),
                bytes: state.mutation_bytes.clone(),
            })
        })
    }

    fn write_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        bytes: &'a [u8],
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async move {
            self.record(
                Action::MutationWrite(request.clone(), bytes.into()),
                snapshot,
            );
            check(cancellation)?;
            let mut state = self.0.lock().unwrap();
            if let Some(error) = &state.reject_write {
                return Err(OperationError::Failed(error.clone()));
            }
            state.mutation_bytes = Some(bytes.into());
            Ok(state
                .mutation_write_target
                .clone()
                .unwrap_or_else(|| target(request)))
        })
    }
}

fn target(request: &MutationRequest) -> MutationTarget {
    MutationTarget {
        physical_path: request.requested.clone(),
        display_path: request.requested.to_string_lossy().into_owned(),
    }
}
