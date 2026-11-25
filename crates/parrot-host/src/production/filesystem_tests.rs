use super::*;

type Security = SharedSecurity<ProductionPolicy>;

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    filesystem::{
        AuthorizedMutationService, AuthorizedReadService, EntryKind, FileReadError,
        FileReadErrorKind, LinkPolicy, MutationRequest, ReadConsumer, ReadRequest,
    },
    security::AccessSnapshot,
};
use parrot_domain::values::project_workspace::ProjectWorkspace;
use parrot_foundation::{
    config::runtime::{SandboxRule, SandboxRuleAction},
    paths::AgentScratchDirectory,
};
use parrot_fs::ProductionFilesystem;
use parrot_runtime::plan_mode::RetainedModePolicyMaterializer;
use parrot_storage::events::NeverCancelled;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn templates() -> Arc<parrot_foundation::template_catalog::PromptTemplateCatalog> {
    let environment = std::collections::BTreeMap::from([
        ("HOME".to_owned(), "/tmp/t55/home".to_owned()),
        ("XDG_CONFIG_HOME".to_owned(), "/tmp/t55/config".to_owned()),
        ("XDG_STATE_HOME".to_owned(), "/tmp/t55/state".to_owned()),
        ("XDG_DATA_HOME".to_owned(), "/tmp/t55/data".to_owned()),
        ("WORKDIR".to_owned(), "/tmp/t55/workspace".to_owned()),
    ]);
    Arc::new(
        parrot_foundation::template_catalog::PromptTemplateCatalog::from_content("", &environment)
            .unwrap()
            .1,
    )
}

struct Fixture {
    root: PathBuf,
    workspace: ProjectWorkspace,
    filesystem: ProductionFilesystem<Security>,
    session: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-fs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        // macOS temporary-directory aliases must not become mutation symlink parents.
        #[cfg(target_os = "macos")]
        let work = fs::canonicalize(work).unwrap();
        #[cfg(target_os = "macos")]
        let root = work.parent().unwrap().to_path_buf();
        let workspace = ProjectWorkspace::from_resolved(work.clone(), work, None, None);
        let filesystem = ProductionFilesystem::new(workspace.clone()).unwrap();
        Self {
            session: root.join("session"),
            root,
            workspace,
            filesystem,
        }
    }
    fn security(&self, rules: &[SandboxRule]) -> Security {
        let materializer = ProductionPolicyMaterializer::new();
        materializer
            .register_session(self.session.clone(), self.workspace.clone())
            .unwrap();
        let scratch =
            AgentScratchDirectory::new(&self.session.join("root-agents/main"), &self.root).unwrap();
        let raw = Arc::new(ProductionPolicy::compose(false, &[], rules, &[]).unwrap());
        SharedSecurity(materializer.for_agent(&raw, &scratch).unwrap())
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.workspace.launch_directory().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn rule(path: &Path, action: SandboxRuleAction) -> SandboxRule {
    SandboxRule {
        path: path.to_string_lossy().into_owned(),
        action,
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
fn error_kind<T>(result: OperationResult<T, FileReadError>) -> FileReadErrorKind {
    match result {
        Err(OperationError::Failed(error)) => error.kind,
        _ => panic!("expected filesystem failure"),
    }
}
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_tmp_alias_supports_mutation_reads_and_denials_without_canonicalizing_fixture() {
    let root = Path::new("/tmp").join(format!(
        "parrot-fs-alias-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let workspace = ProjectWorkspace::from_resolved(work.clone(), work, None, None);
    let fixture = Fixture {
        filesystem: ProductionFilesystem::new(workspace.clone()).unwrap(),
        session: root.join("session"),
        root,
        workspace,
    };
    let physical_root = Path::new("/private").join(fixture.root.strip_prefix("/").unwrap());
    let security = fixture.security(&[]);
    let path = fixture.workspace.launch_directory().join("new/child");
    let create = MutationRequest {
        requested: path.clone(),
        create: true,
    };
    let target = fixture
        .filesystem
        .write_for_mutation(&create, b"created", &security, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(target.physical_path, physical_root.join("work/new/child"));
    assert_eq!(target.display_path, "new/child");
    let edit = MutationRequest {
        requested: PathBuf::from("new/child"),
        create: false,
    };
    let before = fixture
        .filesystem
        .read_for_mutation(&edit, &security, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(before.target, target);
    assert_eq!(before.bytes.unwrap(), b"created");
    fixture
        .filesystem
        .write_for_mutation(&edit, b"edited", &security, &NeverCancelled)
        .await
        .unwrap();
    let read = read_request(&path, LinkPolicy::RejectLinks);
    assert_eq!(
        fixture
            .filesystem
            .read(&read, &security, &NeverCancelled)
            .await
            .unwrap()
            .bytes,
        b"edited"
    );
    let denied = fixture.security(&[
        rule(&path, SandboxRuleAction::DenyRead),
        rule(&path, SandboxRuleAction::DenyWrite),
    ]);
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(&read, &denied, &NeverCancelled)
                .await
        ),
        FileReadErrorKind::Denied
    );
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .write_for_mutation(&edit, b"denied", &denied, &NeverCancelled)
                .await
        ),
        FileReadErrorKind::Denied
    );
    assert_eq!(fs::read(&path).unwrap(), b"edited");
    let link = fixture.workspace.launch_directory().join("link");
    symlink(&path, &link).unwrap();
    assert!(
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: link.clone(),
                    create: false,
                },
                b"linked",
                &security,
                &NeverCancelled
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .filesystem
            .read(
                &read_request(&link, LinkPolicy::RejectLinks),
                &security,
                &NeverCancelled
            )
            .await
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), b"edited");
}

struct Consumer<F>(F);
impl<F: FnMut(&[u8]) -> bool + Send> ReadConsumer for Consumer<F> {
    fn consume(&mut self, bytes: &[u8]) -> bool {
        self.0(bytes)
    }
}

#[tokio::test]
async fn production_reader_streams_sparse_binary_without_consuming_the_tail() {
    let fixture = Fixture::new();
    let path = fixture.file("sparse.bin", b"text\0");
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let security = fixture.security(&[]);
    let reader = super::ProductionReader(fixture.filesystem.clone());
    let mut consumed = 0;
    let mut consumer = Consumer(|bytes: &[u8]| {
        consumed += bytes.len();
        assert!(bytes.contains(&0));
        false
    });
    let physical = reader
        .stream_read(
            &read_request(&path, LinkPolicy::Follow),
            &security.0,
            &NeverCancelled,
            &mut consumer,
        )
        .await
        .unwrap();
    assert_eq!(physical, path);
    assert_eq!(consumed, 8192);

    let tool = parrot_tools::files::ReadTool::new(
        reader,
        parrot_tools::files::WorkspacePaths::default(),
        std::sync::Arc::new(parrot_tools::files::PathTipLedger::enabled()),
        parrot_domain::values::tools::ConfiguredToolDefinition::new("read", "{}"),
    );
    let invocation = parrot_tools::files::FileToolInvocation::new(
        "read-test",
        serde_json::json!({"path": path}).to_string(),
        templates(),
    );
    let result = tool
        .execute(&invocation, &security.0, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(result.text, "error: binary file");
}

#[tokio::test]
async fn production_read_tool_scans_large_text_after_one_output_line() {
    use std::io::Write;
    let fixture = Fixture::new();
    let path = fixture.file("large.txt", b"");
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    let chunk = b"line\n".repeat(1000);
    for _ in 0..1000 {
        file.write_all(&chunk).unwrap();
    }
    drop(file);
    let security = fixture.security(&[]);
    let tool = parrot_tools::files::ReadTool::new(
        fixture.filesystem.clone(),
        parrot_tools::files::WorkspacePaths::default(),
        std::sync::Arc::new(parrot_tools::files::PathTipLedger::enabled()),
        parrot_domain::values::tools::ConfiguredToolDefinition::new("read", "{}"),
    );
    let invocation = parrot_tools::files::FileToolInvocation::new(
        "read-test",
        serde_json::json!({"path": path, "limit": 1}).to_string(),
        templates(),
    );
    let result = tool
        .execute(&invocation, &security, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(result.text, "1: line\ntotal lines in file: 1000000\n");
}

#[tokio::test]
async fn streaming_early_stop_still_validates_the_opened_target() {
    let fixture = Fixture::new();
    let path = fixture.file("binary", b"\0binary");
    let moved = path.with_extension("moved");
    let security = fixture.security(&[]);
    let mut consumer = Consumer(|bytes: &[u8]| {
        assert_eq!(bytes, b"\0binary");
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement").unwrap();
        false
    });
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .stream_read(
                    &read_request(&path, LinkPolicy::Follow),
                    &security,
                    &NeverCancelled,
                    &mut consumer
                )
                .await
        ),
        FileReadErrorKind::TargetChanged
    );
    assert_eq!(fs::read(moved).unwrap(), b"\0binary");
    assert_eq!(fs::read(path).unwrap(), b"replacement");
}

#[tokio::test]
async fn streaming_cancellation_after_consumer_stops_is_propagated() {
    use std::sync::atomic::AtomicBool;
    struct Cancel(AtomicBool);
    impl Cancellation for Cancel {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }
    let fixture = Fixture::new();
    let path = fixture.file("text", b"one\ntwo\n");
    let security = fixture.security(&[]);
    let cancellation = Cancel(AtomicBool::new(false));
    let mut consumer = Consumer(|_: &[u8]| {
        cancellation.0.store(true, Ordering::Relaxed);
        false
    });
    assert!(matches!(
        fixture
            .filesystem
            .stream_read(
                &read_request(&path, LinkPolicy::Follow),
                &security,
                &cancellation,
                &mut consumer
            )
            .await,
        Err(OperationError::Cancelled)
    ));
}

#[tokio::test]
async fn streaming_authorizes_lexical_and_physical_targets_before_consumption() {
    let fixture = Fixture::new();
    let path = fixture.file("text", b"secret");
    let alias = fixture.workspace.launch_directory().join("alias");
    symlink(&path, &alias).unwrap();
    for denied_path in [&path, &alias] {
        let security = fixture.security(&[rule(denied_path, SandboxRuleAction::DenyRead)]);
        let mut consumer = Consumer(|_: &[u8]| panic!("denied bytes must not reach the consumer"));
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .stream_read(
                        &read_request(&alias, LinkPolicy::Follow),
                        &security,
                        &NeverCancelled,
                        &mut consumer
                    )
                    .await
            ),
            FileReadErrorKind::Denied
        );
    }
    let security = fixture.security(&[]);
    let mut request = read_request(&path, LinkPolicy::Follow);
    request.expected_physical = Some(fixture.root.join("different"));
    let mut consumer = Consumer(|_: &[u8]| panic!("changed target must not reach the consumer"));
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .stream_read(&request, &security, &NeverCancelled, &mut consumer)
                .await
        ),
        FileReadErrorKind::TargetChanged
    );
}

#[tokio::test]
async fn allowed_reads_denials_and_actual_byte_limits() {
    let fixture = Fixture::new();
    let path = fixture.file("read.txt", b"hello");
    let security = fixture.security(&[]);
    let result = fixture
        .filesystem
        .read(
            &read_request(Path::new("read.txt"), LinkPolicy::Follow),
            &security,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(result.bytes, b"hello");
    assert_eq!(result.physical_path, path);
    let denied = fixture.security(&[rule(&path, SandboxRuleAction::DenyRead)]);
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(
                    &read_request(&path, LinkPolicy::Follow),
                    &denied,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::Denied
    );
    let mut request = read_request(&path, LinkPolicy::Follow);
    request.max_bytes = Some(4);
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(&request, &security, &NeverCancelled)
                .await
        ),
        FileReadErrorKind::TooLarge
    );
    request.max_bytes = Some(5);
    assert_eq!(
        fixture
            .filesystem
            .read(&request, &security, &NeverCancelled)
            .await
            .unwrap()
            .bytes,
        b"hello"
    );
}

#[tokio::test]
async fn symlinked_workspace_retains_lexical_and_physical_authorization() {
    let fixture = Fixture::new();
    let physical_file = fixture.file("file", b"content");
    let alias = fixture.root.join("workspace-alias");
    symlink(fixture.workspace.launch_directory(), &alias).unwrap();
    let filesystem = ProductionFilesystem::new(ProjectWorkspace::from_resolved(
        alias.clone(),
        fixture.workspace.physical_identity().to_path_buf(),
        None,
        None,
    ))
    .unwrap();
    let request = read_request(Path::new("file"), LinkPolicy::Follow);
    let allowed = fixture.security(&[rule(&alias, SandboxRuleAction::AllowWrite)]);
    let read = filesystem
        .read(&request, &allowed, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(read.lexical_path, alias.join("file"));
    assert_eq!(read.physical_path, physical_file);
    for denied_path in [&alias, fixture.workspace.launch_directory()] {
        let denied = fixture.security(&[rule(denied_path, SandboxRuleAction::DenyRead)]);
        assert_eq!(
            error_kind(filesystem.read(&request, &denied, &NeverCancelled).await),
            FileReadErrorKind::Denied
        );
    }
    assert_eq!(
        error_kind(
            filesystem
                .read(
                    &read_request(Path::new("file"), LinkPolicy::RejectLinks),
                    &allowed,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::InvalidOperation
    );
    assert_eq!(
        error_kind(
            filesystem
                .write_for_mutation(
                    &MutationRequest {
                        requested: "file".into(),
                        create: false
                    },
                    b"bad",
                    &allowed,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::InvalidOperation
    );
    assert_eq!(fs::read(&physical_file).unwrap(), b"content");
}

#[tokio::test]
async fn expected_target_is_checked_and_final_nofollow_is_observed() {
    let fixture = Fixture::new();
    let path = fixture.file("file", b"x");
    let link = fixture.workspace.launch_directory().join("link");
    symlink(&path, &link).unwrap();
    let security = fixture.security(&[]);
    let meta = fixture
        .filesystem
        .inspect(&link, LinkPolicy::NoFollow, &security, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(meta.kind, EntryKind::SymbolicLink);
    assert_eq!(meta.physical_path, link);
    let mut request = read_request(&path, LinkPolicy::Follow);
    request.expected_physical = Some(fixture.root.join("different"));
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(&request, &security, &NeverCancelled)
                .await
        ),
        FileReadErrorKind::TargetChanged
    );
}

#[tokio::test]
async fn follow_cannot_escape_denied_physical_target_and_reject_links_checks_components() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("private");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"secret").unwrap();
    let link = fixture.workspace.launch_directory().join("linked");
    symlink(&outside, &link).unwrap();
    let security = fixture.security(&[rule(&outside, SandboxRuleAction::DenyRead)]);
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(
                    &read_request(&link.join("secret"), LinkPolicy::Follow),
                    &security,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::Denied
    );
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(
                    &read_request(&link.join("secret"), LinkPolicy::RejectLinks),
                    &security,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::InvalidOperation
    );
}

#[tokio::test]
async fn listings_do_not_follow_final_links_or_disclose_denied_names() {
    let fixture = Fixture::new();
    fixture.file("visible", b"x");
    let hidden = fixture.file("denied", b"x");
    symlink(&hidden, fixture.workspace.launch_directory().join("link")).unwrap();
    let security = fixture.security(&[rule(&hidden, SandboxRuleAction::DenyRead)]);
    let entries = fixture
        .filesystem
        .list_directory(
            Path::new("."),
            LinkPolicy::Follow,
            &security,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(!entries.iter().any(|entry| entry.lexical_path == hidden));
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind == EntryKind::SymbolicLink
                && entry.lexical_path.ends_with("link"))
    );
}

#[tokio::test]
async fn mutation_inspection_is_effect_free_and_missing_edit_fails() {
    let fixture = Fixture::new();
    let security = fixture.security(&[]);
    let path = fixture
        .workspace
        .launch_directory()
        .join("new/parents/file");
    let request = MutationRequest {
        requested: path.clone(),
        create: true,
    };
    let target = fixture
        .filesystem
        .inspect_for_mutation(&request, &security, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(target.physical_path, path);
    assert_eq!(target.display_path, "new/parents/file");
    assert!(!path.parent().unwrap().exists());
    assert!(
        fixture
            .filesystem
            .read_for_mutation(&request, &security, &NeverCancelled)
            .await
            .unwrap()
            .bytes
            .is_none()
    );
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .inspect_for_mutation(
                    &MutationRequest {
                        create: false,
                        ..request
                    },
                    &security,
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::NotFound
    );
}

#[tokio::test]
async fn denied_missing_parent_prevents_every_creation() {
    let fixture = Fixture::new();
    let parent = fixture.workspace.launch_directory().join("blocked");
    let path = parent.join("child/file");
    let security = fixture.security(&[
        rule(&parent, SandboxRuleAction::DenyWrite),
        rule(&path, SandboxRuleAction::AllowWrite),
    ]);
    assert!(security.check_write(&path).is_ok());
    let result = fixture
        .filesystem
        .write_for_mutation(
            &MutationRequest {
                requested: path,
                create: true,
            },
            b"x",
            &security,
            &NeverCancelled,
        )
        .await;
    assert_eq!(error_kind(result), FileReadErrorKind::Denied);
    assert!(!parent.exists());
}

#[tokio::test]
async fn overwrite_preserves_mode_and_inode_and_creation_is_private() {
    let fixture = Fixture::new();
    let path = fixture.file("existing", b"old contents");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&path).unwrap();
    let security = fixture.security(&[]);
    fixture
        .filesystem
        .write_for_mutation(
            &MutationRequest {
                requested: path.clone(),
                create: false,
            },
            b"new",
            &security,
            &NeverCancelled,
        )
        .await
        .unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(after.mode() & 0o777, 0o640);
    assert_eq!(fs::read(&path).unwrap(), b"new");
    let created = fixture.workspace.launch_directory().join("a/b/new");
    fixture
        .filesystem
        .write_for_mutation(
            &MutationRequest {
                requested: created.clone(),
                create: true,
            },
            b"private",
            &security,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(fs::metadata(&created).unwrap().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn write_policy_not_read_policy_controls_mutation_reads() {
    let fixture = Fixture::new();
    let security = fixture.security(&[rule(&fixture.session, SandboxRuleAction::DenyRead)]);
    let path = fixture.session.join("owned");
    fs::write(&path, b"owned").unwrap();
    assert!(!security.snapshot().allows_read(&path));
    assert!(security.snapshot().allows_write(&path));
    assert_eq!(
        fixture
            .filesystem
            .read_for_mutation(
                &MutationRequest {
                    requested: path.clone(),
                    create: false
                },
                &security,
                &NeverCancelled
            )
            .await
            .unwrap()
            .bytes
            .unwrap(),
        b"owned"
    );
    fixture
        .filesystem
        .write_for_mutation(
            &MutationRequest {
                requested: path.clone(),
                create: false,
            },
            b"updated",
            &security,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"updated");
}

#[tokio::test]
async fn mutation_rejects_final_and_intermediate_links_without_touching_targets() {
    let fixture = Fixture::new();
    let path = fixture.file("target", b"original");
    let link = fixture.workspace.launch_directory().join("link");
    symlink(&path, &link).unwrap();
    let security = fixture.security(&[]);
    assert!(
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: link,
                    create: true
                },
                b"bad",
                &security,
                &NeverCancelled
            )
            .await
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");
    let directory_link = fixture.workspace.launch_directory().join("directory-link");
    symlink(fixture.workspace.launch_directory(), &directory_link).unwrap();
    assert!(
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: directory_link.join("new"),
                    create: true
                },
                b"bad",
                &security,
                &NeverCancelled
            )
            .await
            .is_err()
    );
    assert!(!fixture.workspace.launch_directory().join("new").exists());
}

#[tokio::test]
async fn preflight_then_swapped_link_is_rejected_before_truncation() {
    let fixture = Fixture::new();
    let path = fixture.file("original", b"original");
    let victim = fixture.file("victim", b"victim");
    let security = fixture.security(&[]);
    let request = MutationRequest {
        requested: path.clone(),
        create: false,
    };
    fixture
        .filesystem
        .inspect_for_mutation(&request, &security, &NeverCancelled)
        .await
        .unwrap();
    let moved = fixture.workspace.launch_directory().join("moved");
    fs::rename(&path, &moved).unwrap();
    symlink(&victim, &path).unwrap();
    assert!(
        fixture
            .filesystem
            .write_for_mutation(&request, b"bad", &security, &NeverCancelled)
            .await
            .is_err()
    );
    assert_eq!(fs::read(&victim).unwrap(), b"victim");
    assert_eq!(fs::read(&moved).unwrap(), b"original");
}

#[tokio::test]
async fn fifo_read_is_rejected_without_opening_a_blocking_reader() {
    let fixture = Fixture::new();
    let path = fixture.workspace.launch_directory().join("fifo");
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(
        error_kind(
            fixture
                .filesystem
                .read(
                    &read_request(&path, LinkPolicy::Follow),
                    &fixture.security(&[]),
                    &NeverCancelled
                )
                .await
        ),
        FileReadErrorKind::InvalidOperation
    );
}
