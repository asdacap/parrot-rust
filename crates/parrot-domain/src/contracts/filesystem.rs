//! I/O obligations for injected, workspace-bound adapters.
//!
//! This module supplies no filesystem or authorization implementation. Adapters
//! must resolve, authorize and perform each operation together under the supplied
//! immutable invocation snapshot. Metadata and returned paths are reports, not
//! capabilities. Nothing here authorizes writes, grants, fallback or re-following
//! a reported target in a separate unchecked operation.

use std::fmt;
use std::path::{Path, PathBuf};

use super::async_ops::{BoxFuture, Cancellation, OperationResult};
use super::security::AccessSnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkPolicy {
    /// Reject a symbolic link in any component, as required for task artifacts.
    RejectLinks,
    /// Inspect the final entry itself; do not read through a final symbolic link.
    /// Directory listings also report children without following their links.
    NoFollow,
    /// Resolve links subject to authorization of lexical and physical targets.
    Follow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Regular,
    Directory,
    SymbolicLink,
    Other,
}

/// An observation, not permission to subsequently open or follow either path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub lexical_path: PathBuf,
    /// With `NoFollow`, a symbolic link is reported as the link, not its target.
    pub physical_path: PathBuf,
    pub kind: EntryKind,
    pub hidden: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub requested: PathBuf,
    /// If supplied, the opened descriptor must still identify this target.
    pub expected_physical: Option<PathBuf>,
    pub links: LinkPolicy,
    /// Limit actual bytes returned, not merely a prior metadata size. `None`
    /// preserves the unbounded source artifact read; skills request 1 MiB.
    pub max_bytes: Option<usize>,
}

/// Bytes from the one validated regular-file operation. Callers apply their
/// source-specific decoding (strict skill UTF-8 versus artifact StreamReader
/// BOM detection/replacement) before parsing. These paths confer no authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundRead {
    pub lexical_path: PathBuf,
    pub physical_path: PathBuf,
    pub bytes: Vec<u8>,
}

/// Categories preserve source catch boundaries rather than normalizing all
/// failures to absence or denial. Adapters retain the actual source message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileReadErrorKind {
    NotFound,
    Denied,
    InvalidPath,
    NotSupported,
    PathTooLong,
    InvalidOperation,
    InvalidData,
    Io,
    /// The skill size limit is an InvalidDataException (an IOException).
    TooLarge,
    /// An opened-target mismatch is an IOException, not an authorization grant.
    TargetChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileReadError {
    pub kind: FileReadErrorKind,
    pub message: String,
}

impl FileReadError {
    pub fn new(kind: FileReadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The source IOException family. Denied remains UnauthorizedAccessException;
    /// InvalidPath, NotSupported and InvalidOperation retain separate boundaries.
    pub fn is_io(&self) -> bool {
        matches!(
            self.kind,
            FileReadErrorKind::NotFound
                | FileReadErrorKind::PathTooLong
                | FileReadErrorKind::InvalidData
                | FileReadErrorKind::Io
                | FileReadErrorKind::TooLarge
                | FileReadErrorKind::TargetChanged
        )
    }
}

impl fmt::Display for FileReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FileReadError {}

/// Receives sequential chunks of at most 8192 bytes from one authorized read.
/// Returning `false` stops consumption, but does not skip final target validation.
pub trait ReadConsumer: Send {
    fn consume(&mut self, bytes: &[u8]) -> bool;
}

/// A provider bound to a workspace/path environment, not a general privileged
/// filesystem handle. It must use the supplied captured snapshot, never recapture
/// mutable policy or assume that constructing `ResolvedRead` authorizes access.
/// Production resolution/authorization adapters remain separate obligations.
pub trait AuthorizedReadService: Send + Sync {
    type Security: AccessSnapshot;

    /// Resolve and authorize this observation. Absence is `NotFound`, distinct
    /// from denied, corrupt, invalid and I/O failures.
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>>;

    /// Resolve/authorize the directory and returned entry observations. Return
    /// unsorted children without following their final links; callers implement
    /// source ordinal sorting, traversal limits and explicit follow decisions.
    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>>;

    /// Resolve, open, validate, authorize and read one regular file. The adapter
    /// owns descriptor lifetime and checks regularity, link policy, lexical and
    /// physical authorization, opened-target identity and any expected target
    /// before reading. Enforce the requested byte limit during this operation;
    /// a size check alone is insufficient. Do not silently truncate or fall back.
    ///
    /// `RejectLinks` includes every path component and the opened descriptor
    /// identity (ToolWorkspace.OpenRegularReadWithoutLinks). Skill reads bind
    /// the discovered physical target and 1 MiB limit (SkillFileReader.Read).
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>>;

    /// Stream one descriptor-bound regular-file operation with the same checks
    /// and byte limit as `read`, including final validation after a consumer
    /// stops early. Returns the actual bound physical path, not a capability.
    /// Production adapters must override this method to avoid buffering the file.
    /// This default only preserves compatibility with existing buffered adapters.
    fn stream_read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
        consumer: &'a mut dyn ReadConsumer,
    ) -> BoxFuture<'a, OperationResult<PathBuf, FileReadError>> {
        Box::pin(async move {
            let read = self.read(request, snapshot, cancellation).await?;
            for chunk in read.bytes.chunks(8192) {
                cancellation
                    .check()
                    .map_err(|_| super::async_ops::OperationError::Cancelled)?;
                if !consumer.consume(chunk) {
                    break;
                }
            }
            cancellation
                .check()
                .map_err(|_| super::async_ops::OperationError::Cancelled)?;
            Ok(read.physical_path)
        })
    }
}

/// The already-expanded caller path and source Write (`true`) / Edit (`false`)
/// mode. This request carries no authorization or resolved-target capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationRequest {
    pub requested: PathBuf,
    pub create: bool,
}

/// Reports from one authorized operation, never permission for unchecked I/O.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationTarget {
    pub physical_path: PathBuf,
    pub display_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationRead {
    pub target: MutationTarget,
    /// `None` is only a missing file in create mode. Edit-mode absence is an error.
    pub bytes: Option<Vec<u8>>,
}

/// Workspace-bound mutation operations under one captured invocation snapshot.
/// No backend or default implementation is supplied here. Every operation must
/// independently resolve the original requested path, reject links in every
/// component and nonregular final entries, and authorize lexical/actual targets
/// using WRITE policy, including any missing parents in create mode. A prior
/// report or `AccessSnapshot::check_write` alone never authorizes later I/O.
///
/// Implementations own descriptor validation/lifetime and cancellation before
/// effects. They must not replace these obligations with caller-side `std::fs`
/// access, nor add compare-and-swap semantics absent from the source behavior.
/// Production filesystem security remains a separate, unproved adapter.
pub trait AuthorizedMutationService: Send + Sync {
    type Security: AccessSnapshot;

    /// Validate only: do not read existing contents, create parents, truncate or
    /// otherwise mutate. Writable-but-unreadable regular output is valid. Missing
    /// files are valid only in create mode; missing-parent authorization still
    /// occurs before returning. Used before image generation/provider work.
    fn inspect_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>>;

    /// Resolve/authorize, validate the opened actual target and read regular-file
    /// bytes under WRITE policy (not an additional read-policy requirement).
    /// Create-mode absence returns `None`; no parents or file are created.
    fn read_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>>;

    /// Repeat resolution/authorization of the ORIGINAL requested path and bind
    /// actual-target checks to the descriptor before any creation/truncation or
    /// write. Create parents only in create mode, after their authorization.
    /// Existing regular contents may be overwritten without read permission.
    /// Return the final actual/display paths used by the successful operation.
    /// Callers pass the same captured snapshot as preflight/read; unchanged byte
    /// transforms skip this operation entirely.
    fn write_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        bytes: &'a [u8],
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::async_ops::OperationError;
    use crate::contracts::paths::ResolvedRead;
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    // Signature fixture only: it has no filesystem implementation and cannot
    // produce a successful observation. This is not read-security evidence.
    struct DeniedSnapshot;
    impl AccessSnapshot for DeniedSnapshot {
        type Error = FileReadError;

        fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
            Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied",
            ))
        }

        fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
            Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied",
            ))
        }
    }

    struct UnavailableReader;
    impl AuthorizedReadService for UnavailableReader {
        type Security = DeniedSnapshot;

        fn inspect<'a>(
            &'a self,
            _: &'a Path,
            _: LinkPolicy,
            _: &'a Self::Security,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
            Box::pin(async {
                Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::NotFound,
                    "fixture missing",
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
                    FileReadErrorKind::NotFound,
                    "fixture missing",
                )))
            })
        }

        fn read<'a>(
            &'a self,
            _: &'a ReadRequest,
            _: &'a Self::Security,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
            Box::pin(async {
                Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Denied,
                    "fixture denied",
                )))
            })
        }
    }

    struct NeverCancelled;
    impl Cancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }

        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn ready<T>(mut future: BoxFuture<'_, T>) -> T {
        let waker = Waker::from(Arc::new(NoopWake));
        match Future::poll(future.as_mut(), &mut Context::from_waker(&waker)) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("signature fixture must be immediately ready"),
        }
    }

    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    #[test]
    fn denied_only_service_signatures_are_send_and_preserve_failure_classes() {
        let service = UnavailableReader;
        let snapshot = DeniedSnapshot;
        let cancellation = NeverCancelled;
        let path = Path::new("fixture");
        let request = ReadRequest {
            requested: path.into(),
            expected_physical: None,
            links: LinkPolicy::RejectLinks,
            max_bytes: None,
        };
        for error in [
            ready(assert_send(service.inspect(
                path,
                LinkPolicy::NoFollow,
                &snapshot,
                &cancellation,
            )))
            .unwrap_err(),
            ready(assert_send(service.list_directory(
                path,
                LinkPolicy::Follow,
                &snapshot,
                &cancellation,
            )))
            .unwrap_err(),
        ] {
            assert!(
                matches!(error, OperationError::Failed(error) if error.kind == FileReadErrorKind::NotFound)
            );
        }
        assert!(matches!(
            ready(assert_send(service.read(&request, &snapshot, &cancellation))),
            Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::Denied
        ));
    }

    #[test]
    fn source_error_families_and_messages_are_not_collapsed() {
        for kind in [
            FileReadErrorKind::NotFound,
            FileReadErrorKind::PathTooLong,
            FileReadErrorKind::InvalidData,
            FileReadErrorKind::Io,
            FileReadErrorKind::TooLarge,
            FileReadErrorKind::TargetChanged,
        ] {
            let error = FileReadError::new(kind, "actual source message");
            assert!(error.is_io());
            assert_eq!(error.to_string(), "actual source message");
        }
        for kind in [
            FileReadErrorKind::Denied,
            FileReadErrorKind::InvalidPath,
            FileReadErrorKind::NotSupported,
            FileReadErrorKind::InvalidOperation,
        ] {
            assert!(!FileReadError::new(kind, "source message").is_io());
        }
    }
}
