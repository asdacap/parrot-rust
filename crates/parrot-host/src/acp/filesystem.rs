//! File byte transfer through the connected ACP client, behind the same
//! authorization the local filesystem service applies.
//!
//! [`AcpFilesystem`] wraps the per-session filesystem service. Every operation
//! first authorizes through the wrapped service, so the security profile stays
//! the single enforcement point. When the session's ACP client advertised the
//! matching fs capability and the adapter registered an access handle, text
//! reads and writes then move their bytes through `fs/read_text_file` and
//! `fs/write_text_file` on the client, so the editor applies and audits its own
//! gating. Sessions without a registered handle behave exactly like the
//! unwrapped service, which keeps CLI and web sessions unchanged.
//!
//! ACP text transfers carry UTF-8 strings, so a routed write rejects
//! non-UTF-8 bytes instead of silently falling back to the local path.
//!
//! Routed operations authorize the resolved path before the client acts, but
//! the client opens the file itself, so a file swapped between authorization
//! and the client's read is not descriptor-pinned the way the local path is.
//! The client is the editor serving the same machine's files under its own
//! gating, which is the point of routing through it.

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    filesystem::{
        AuthorizedMutationService, AuthorizedReadService, BoundRead, FileMetadata, FileReadError,
        FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest, MutationTarget, ReadConsumer,
        ReadRequest,
    },
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

/// Moves file bytes between the agent and the connected ACP client.
pub trait ClientFileAccess: Send + Sync {
    /// Fetches the whole text content of one file through the client.
    fn read_text_file<'a>(
        &'a self,
        path: PathBuf,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, FileReadError>>;

    /// Writes the whole text content of one file through the client, which
    /// creates the file when it is missing.
    fn write_text_file<'a>(
        &'a self,
        path: PathBuf,
        content: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), FileReadError>>;
}

/// Maps one user session to the ACP client that serves its file access.
///
/// Entries live exactly as long as their [`Registration`] guard, which the
/// adapter keeps for the lifetime of its connection, so a disconnected editor
/// falls back to the local path immediately.
pub struct AcpFileRegistry {
    entries: Mutex<HashMap<String, Arc<dyn ClientFileAccess>>>,
}

impl AcpFileRegistry {
    /// The process-wide registry. One ACP agent connection serves a process,
    /// and entries are keyed by user session id, so concurrent sessions cannot
    /// collide.
    pub fn shared() -> Arc<Self> {
        static REGISTRY: OnceLock<Arc<AcpFileRegistry>> = OnceLock::new();
        REGISTRY.get_or_init(|| Arc::new(Self::default())).clone()
    }

    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn register(
        &self,
        session_id: impl Into<String>,
        access: Arc<dyn ClientFileAccess>,
    ) -> Registration {
        let session_id = session_id.into();
        self.entries
            .lock()
            .expect("acp file registry poisoned")
            .insert(session_id.clone(), access);
        Registration {
            registry: AcpFileRegistry::shared(),
            session_id,
        }
    }
    fn access(&self, session_id: &str) -> Option<Arc<dyn ClientFileAccess>> {
        self.entries
            .lock()
            .expect("acp file registry poisoned")
            .get(session_id)
            .cloned()
    }
}

/// Keeps one registry entry alive; dropping it removes the entry.
pub(crate) struct Registration {
    registry: Arc<AcpFileRegistry>,
    session_id: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .entries
            .lock()
            .expect("acp file registry poisoned")
            .remove(&self.session_id);
    }
}

/// The filesystem service for one session: local authorization always, with
/// byte movement through the ACP client when the session registered one.
pub struct AcpFilesystem<F> {
    local: F,
    session_id: String,
    registry: Arc<AcpFileRegistry>,
}

impl<F> AcpFilesystem<F> {
    pub fn new(local: F, session_id: String, registry: Arc<AcpFileRegistry>) -> Self {
        Self {
            local,
            session_id,
            registry,
        }
    }

    fn routed(&self) -> Option<Arc<dyn ClientFileAccess>> {
        self.registry.access(&self.session_id)
    }
}

impl<F: Clone> Clone for AcpFilesystem<F> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            session_id: self.session_id.clone(),
            registry: self.registry.clone(),
        }
    }
}

fn not_regular() -> FileReadError {
    FileReadError::new(
        FileReadErrorKind::InvalidOperation,
        "not a regular file".to_owned(),
    )
}

pub(crate) fn transfer_failed(error: impl std::fmt::Display) -> FileReadError {
    FileReadError::new(FileReadErrorKind::Io, format!("acp client: {error}"))
}

fn too_large(limit: usize) -> FileReadError {
    FileReadError::new(
        FileReadErrorKind::TooLarge,
        format!("content exceeds the {limit} byte limit"),
    )
}

impl<F: AuthorizedReadService> AuthorizedReadService for AcpFilesystem<F> {
    type Security = F::Security;

    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        self.local.inspect(path, links, snapshot, cancellation)
    }

    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        self.local
            .list_directory(path, links, snapshot, cancellation)
    }

    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            let Some(access) = self.routed() else {
                return self.local.read(request, snapshot, cancellation).await;
            };
            let metadata = self
                .local
                .inspect(&request.requested, request.links, snapshot, cancellation)
                .await?;
            if let Some(expected) = &request.expected_physical
                && expected != &metadata.physical_path
            {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TargetChanged,
                    "the resolved target no longer matches the observed one",
                )));
            }
            if metadata.kind != parrot_domain::contracts::filesystem::EntryKind::Regular {
                return Err(OperationError::Failed(not_regular()));
            }
            let content = access
                .read_text_file(metadata.physical_path.clone(), cancellation)
                .await?;
            let bytes = content.into_bytes();
            if let Some(limit) = request.max_bytes
                && bytes.len() > limit
            {
                return Err(OperationError::Failed(too_large(limit)));
            }
            Ok(BoundRead {
                lexical_path: metadata.lexical_path,
                physical_path: metadata.physical_path,
                bytes,
            })
        })
    }

    fn stream_read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
        consumer: &'a mut dyn ReadConsumer,
    ) -> BoxFuture<'a, OperationResult<PathBuf, FileReadError>> {
        Box::pin(async move {
            if self.routed().is_none() {
                return self
                    .local
                    .stream_read(request, snapshot, cancellation, consumer)
                    .await;
            }
            let read = self.read(request, snapshot, cancellation).await?;
            for chunk in read.bytes.chunks(8192) {
                cancellation
                    .check()
                    .map_err(|_| OperationError::Cancelled)?;
                if !consumer.consume(chunk) {
                    break;
                }
            }
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            Ok(read.physical_path)
        })
    }
}

impl<F> AuthorizedMutationService for AcpFilesystem<F>
where
    F: AuthorizedMutationService,
    F: AuthorizedReadService<Security = <F as AuthorizedMutationService>::Security>,
{
    type Security = <F as AuthorizedMutationService>::Security;

    fn inspect_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        self.local
            .inspect_for_mutation(request, snapshot, cancellation)
    }

    fn read_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>> {
        Box::pin(async move {
            let Some(access) = self.routed() else {
                return self
                    .local
                    .read_for_mutation(request, snapshot, cancellation)
                    .await;
            };
            let target = self
                .local
                .inspect_for_mutation(request, snapshot, cancellation)
                .await?;
            match self
                .local
                .inspect(
                    &request.requested,
                    LinkPolicy::RejectLinks,
                    snapshot,
                    cancellation,
                )
                .await
            {
                Ok(metadata)
                    if metadata.kind
                        == parrot_domain::contracts::filesystem::EntryKind::Regular =>
                {
                    let content = access
                        .read_text_file(metadata.physical_path.clone(), cancellation)
                        .await?;
                    Ok(MutationRead {
                        target,
                        bytes: Some(content.into_bytes()),
                    })
                }
                Ok(_) => Err(OperationError::Failed(not_regular())),
                Err(OperationError::Failed(error))
                    if error.kind == FileReadErrorKind::NotFound && request.create =>
                {
                    Ok(MutationRead {
                        target,
                        bytes: None,
                    })
                }
                Err(error) => Err(error),
            }
        })
    }

    fn write_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        bytes: &'a [u8],
        snapshot: &'a Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async move {
            let Some(access) = self.routed() else {
                return self
                    .local
                    .write_for_mutation(request, bytes, snapshot, cancellation)
                    .await;
            };
            let content = std::str::from_utf8(bytes).map_err(|_| {
                OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::InvalidData,
                    "only UTF-8 writes route through the acp client",
                ))
            })?;
            // Authorization including missing-parent checks in create mode,
            // then the client performs the write under its own gating.
            let target = self
                .local
                .inspect_for_mutation(request, snapshot, cancellation)
                .await?;
            access
                .write_text_file(
                    target.physical_path.clone(),
                    content.to_owned(),
                    cancellation,
                )
                .await?;
            Ok(target)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::contracts::filesystem::EntryKind;
    use parrot_domain::values::project_workspace::ProjectWorkspace;
    use parrot_foundation::{
        config::runtime::{SandboxRule, SandboxRuleAction},
        paths::AgentScratchDirectory,
    };
    use parrot_fs::ProductionFilesystem;
    use parrot_runtime::plan_mode::RetainedModePolicyMaterializer;
    use parrot_runtime::policy::{ProductionPolicy, ProductionPolicyMaterializer};
    use parrot_runtime::tool_settlement::SharedSecurity;
    use parrot_storage::events::NeverCancelled;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    type Security = SharedSecurity<ProductionPolicy>;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct FakeAccess {
        reads: Mutex<Vec<PathBuf>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
        content: &'static str,
    }
    impl FakeAccess {
        fn new(content: &'static str) -> Self {
            Self {
                reads: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
                content,
            }
        }
    }
    impl ClientFileAccess for FakeAccess {
        fn read_text_file<'a>(
            &'a self,
            path: PathBuf,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<String, FileReadError>> {
            Box::pin(async move {
                self.reads.lock().unwrap().push(path);
                Ok(self.content.to_owned())
            })
        }

        fn write_text_file<'a>(
            &'a self,
            path: PathBuf,
            content: String,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<(), FileReadError>> {
            Box::pin(async move {
                self.writes.lock().unwrap().push((path, content));
                Ok(())
            })
        }
    }

    struct Fixture {
        root: PathBuf,
        workspace: ProjectWorkspace,
        local: ProductionFilesystem<Security>,
        session: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "parrot-acp-fs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let work = root.join("work");
            fs::create_dir_all(&work).unwrap();
            let workspace = ProjectWorkspace::from_resolved(work.clone(), work, None, None);
            let local = ProductionFilesystem::new(workspace.clone()).unwrap();
            Self {
                session: root.join("session"),
                root,
                workspace,
                local,
            }
        }

        fn security(&self, rules: &[SandboxRule]) -> Security {
            let materializer = ProductionPolicyMaterializer::new();
            materializer
                .register_session(self.session.clone(), self.workspace.clone())
                .unwrap();
            let scratch =
                AgentScratchDirectory::new(&self.session.join("root-agents/main"), &self.root)
                    .unwrap();
            let raw = Arc::new(ProductionPolicy::compose(false, &[], rules, &[]).unwrap());
            SharedSecurity(materializer.for_agent(&raw, &scratch).unwrap())
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.workspace.launch_directory().join(name);
            fs::write(&path, bytes).unwrap();
            path
        }

        fn filesystem(&self, session_id: &str) -> AcpFilesystem<ProductionFilesystem<Security>> {
            AcpFilesystem::new(
                self.local.clone(),
                session_id.to_owned(),
                AcpFileRegistry::shared(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn read_request(path: &Path, links: LinkPolicy) -> ReadRequest {
        ReadRequest {
            requested: path.to_path_buf(),
            expected_physical: None,
            links,
            max_bytes: None,
        }
    }

    #[tokio::test]
    async fn routed_read_moves_bytes_through_the_client() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"local bytes");
        let security = fixture.security(&[]);
        let access = Arc::new(FakeAccess::new("remote bytes"));
        let registration = AcpFileRegistry::shared().register(&session, access.clone());
        let filesystem = fixture.filesystem(&session);

        let read = filesystem
            .read(
                &read_request(&path, LinkPolicy::Follow),
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(read.bytes, b"remote bytes");
        assert_eq!(read.physical_path, path);
        assert_eq!(access.reads.lock().unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), b"local bytes");
        drop(registration);
    }

    #[tokio::test]
    async fn sessions_without_a_registered_client_read_locally() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"local bytes");
        let security = fixture.security(&[]);
        let access = Arc::new(FakeAccess::new("remote bytes"));
        let registered = unique_session();
        let _registration = AcpFileRegistry::shared().register(&registered, access.clone());
        let filesystem = fixture.filesystem(&session);

        let read = filesystem
            .read(
                &read_request(&path, LinkPolicy::Follow),
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(read.bytes, b"local bytes");
        assert!(access.reads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_the_registration_restores_the_local_path() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"local bytes");
        let security = fixture.security(&[]);
        let filesystem = fixture.filesystem(&session);
        let registration =
            AcpFileRegistry::shared().register(&session, Arc::new(FakeAccess::new("remote")));
        drop(registration);

        let read = filesystem
            .read(
                &read_request(&path, LinkPolicy::Follow),
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(read.bytes, b"local bytes");
    }

    #[tokio::test]
    async fn routed_write_sends_content_to_the_client() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"before");
        let security = fixture.security(&[]);
        let access = Arc::new(FakeAccess::new(""));
        let _registration = AcpFileRegistry::shared().register(&session, access.clone());
        let filesystem = fixture.filesystem(&session);
        let request = MutationRequest {
            requested: path.clone(),
            create: false,
        };

        filesystem
            .write_for_mutation(&request, b"after", &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(
            access.writes.lock().unwrap().as_slice(),
            [(path.clone(), "after".to_owned())]
        );
        assert_eq!(fs::read(&path).unwrap(), b"before");
    }

    #[tokio::test]
    async fn routed_write_still_requires_write_authorization() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"before");
        let security = fixture.security(&[rule(&path, SandboxRuleAction::DenyWrite)]);
        let access = Arc::new(FakeAccess::new(""));
        let _registration = AcpFileRegistry::shared().register(&session, access.clone());
        let filesystem = fixture.filesystem(&session);
        let request = MutationRequest {
            requested: path.clone(),
            create: false,
        };

        let result = filesystem
            .write_for_mutation(&request, b"after", &security, &NeverCancelled)
            .await;
        assert!(
            matches!(
                result,
                Err(OperationError::Failed(ref error)) if error.kind == FileReadErrorKind::Denied
            ),
            "expected a denial, got {result:?}"
        );
        assert!(access.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn routed_read_enforces_the_requested_byte_limit() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"local");
        let security = fixture.security(&[]);
        let _registration =
            AcpFileRegistry::shared().register(&session, Arc::new(FakeAccess::new("0123456789")));
        let filesystem = fixture.filesystem(&session);
        let mut request = read_request(&path, LinkPolicy::Follow);
        request.max_bytes = Some(4);

        let result = filesystem.read(&request, &security, &NeverCancelled).await;
        assert!(
            matches!(
                result,
                Err(OperationError::Failed(ref error)) if error.kind == FileReadErrorKind::TooLarge
            ),
            "expected a size failure, got {result:?}"
        );
    }

    #[tokio::test]
    async fn routed_mutation_read_distinguishes_missing_files_in_create_mode() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.workspace.launch_directory().join("missing.md");
        let security = fixture.security(&[]);
        let _registration =
            AcpFileRegistry::shared().register(&session, Arc::new(FakeAccess::new("remote")));
        let filesystem = fixture.filesystem(&session);

        let create = MutationRequest {
            requested: path.clone(),
            create: true,
        };
        let read = filesystem
            .read_for_mutation(&create, &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(read.bytes, None);

        let edit = MutationRequest {
            requested: path,
            create: false,
        };
        let result = filesystem
            .read_for_mutation(&edit, &security, &NeverCancelled)
            .await;
        assert!(
            matches!(
                result,
                Err(OperationError::Failed(ref error)) if error.kind == FileReadErrorKind::NotFound
            ),
            "expected absence, got {result:?}"
        );
    }

    #[tokio::test]
    async fn unregistered_mutation_reads_and_writes_stay_local() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"before");
        let security = fixture.security(&[]);
        let filesystem = fixture.filesystem(&session);
        let request = MutationRequest {
            requested: path.clone(),
            create: false,
        };

        let read = filesystem
            .read_for_mutation(&request, &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(read.bytes.as_deref(), Some(b"before".as_slice()));
        filesystem
            .write_for_mutation(&request, b"after", &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"after");
    }

    #[tokio::test]
    async fn routed_read_rejects_expected_target_mismatches() {
        let fixture = Fixture::new();
        let session = unique_session();
        let path = fixture.file("notes.md", b"local");
        let security = fixture.security(&[]);
        let _registration =
            AcpFileRegistry::shared().register(&session, Arc::new(FakeAccess::new("remote")));
        let filesystem = fixture.filesystem(&session);
        let metadata = filesystem
            .inspect(&path, LinkPolicy::Follow, &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(metadata.kind, EntryKind::Regular);
        let mut request = read_request(&path, LinkPolicy::Follow);
        request.expected_physical = Some(metadata.physical_path.with_extension("moved"));

        let result = filesystem.read(&request, &security, &NeverCancelled).await;
        assert!(
            matches!(
                result,
                Err(OperationError::Failed(ref error)) if error.kind == FileReadErrorKind::TargetChanged
            ),
            "expected a target mismatch, got {result:?}"
        );
    }

    fn unique_session() -> String {
        format!("acp-test-{}", NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn rule(path: &Path, action: SandboxRuleAction) -> SandboxRule {
        SandboxRule {
            path: path.to_string_lossy().into_owned(),
            action,
        }
    }
}
