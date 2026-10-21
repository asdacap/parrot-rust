use super::*;
#[path = "agent_skills.rs"]
mod agent_skills;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, EntryKind, FileMetadata, FileReadError,
            FileReadErrorKind, LinkPolicy, ReadRequest,
        },
        paths::{ReadOrigin, ResolvedRead},
        security::AccessSnapshot,
    },
    values::{
        config::{PathPlatform, SkillConfigEntry, SkillConfiguration},
        project_workspace::ProjectWorkspace,
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
struct Policy {
    id: u32,
    denied: Option<PathBuf>,
}
impl AccessSnapshot for Policy {
    type Error = FileReadError;
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Self::Error> {
        if self
            .denied
            .as_ref()
            .is_some_and(|path| read.actual_path().starts_with(path))
        {
            Err(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture denied",
            ))
        } else {
            Ok(())
        }
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        panic!("discovery never writes")
    }
}
fn policy() -> Policy {
    Policy {
        id: 41,
        denied: None,
    }
}
struct Cancel(AtomicBool);
impl Cancel {
    fn no() -> Self {
        Self(AtomicBool::new(false))
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Clone)]
struct Node {
    kind: EntryKind,
    hidden: bool,
    bytes: Vec<u8>,
    target: Option<PathBuf>,
}
#[derive(Clone, Debug)]
struct Call {
    operation: &'static str,
    path: PathBuf,
    links: LinkPolicy,
    policy: u32,
    request: Option<ReadRequest>,
}
#[derive(Default)]
struct Fake {
    nodes: Mutex<BTreeMap<PathBuf, Node>>,
    calls: Mutex<Vec<Call>>,
    errors: Mutex<BTreeMap<(&'static str, PathBuf), FileReadError>>,
    cancel_at: Mutex<Option<(&'static str, PathBuf)>>,
}
impl Fake {
    fn node(
        &self,
        path: &str,
        kind: EntryKind,
        bytes: Vec<u8>,
        target: Option<&str>,
        hidden: bool,
    ) {
        self.nodes.lock().unwrap().insert(
            path.into(),
            Node {
                kind,
                bytes,
                target: target.map(PathBuf::from),
                hidden,
            },
        );
    }
    fn dir(&self, path: &str) {
        self.node(path, EntryKind::Directory, vec![], None, false);
    }
    fn file(&self, path: &str, bytes: impl Into<Vec<u8>>) {
        self.node(path, EntryKind::Regular, bytes.into(), None, false);
    }
    fn skill(&self, path: &str, name: &str) {
        let parent = Path::new(path).parent().unwrap().to_str().unwrap();
        self.dir(parent);
        self.file(
            path,
            format!("---\nname: {name}\ndescription: description\n---\nbody"),
        );
    }
    fn link(&self, path: &str, target: &str) {
        self.node(path, EntryKind::SymbolicLink, vec![], Some(target), false);
    }
    fn error(&self, operation: &'static str, path: &str, kind: FileReadErrorKind, message: &str) {
        self.errors
            .lock()
            .unwrap()
            .insert((operation, path.into()), FileReadError::new(kind, message));
    }
    fn record(
        &self,
        operation: &'static str,
        path: &Path,
        links: LinkPolicy,
        policy: &Policy,
        cancel: &dyn Cancellation,
        request: Option<ReadRequest>,
    ) -> OperationResult<(), FileReadError> {
        cancel.check().map_err(|_| OperationError::Cancelled)?;
        self.calls.lock().unwrap().push(Call {
            operation,
            path: path.into(),
            links,
            policy: policy.id,
            request,
        });
        if self
            .cancel_at
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(op, target)| *op == operation && target == path)
        {
            return Err(OperationError::Cancelled);
        }
        if let Some(error) = self.errors.lock().unwrap().get(&(operation, path.into())) {
            return Err(OperationError::Failed(error.clone()));
        }
        Ok(())
    }
    fn resolve(&self, path: &Path, follow_final: bool) -> Result<(PathBuf, Node), FileReadError> {
        let nodes = self.nodes.lock().unwrap();
        let mut current = path.to_path_buf();
        for _ in 0..32 {
            let mut replaced = None;
            for ancestor in current.ancestors() {
                if ancestor == current && !follow_final {
                    continue;
                }
                if let Some(node) = nodes.get(ancestor) {
                    if let Some(target) = &node.target {
                        replaced = Some(target.join(current.strip_prefix(ancestor).unwrap()));
                    }
                }
            }
            if let Some(path) = replaced {
                current = path;
                continue;
            }
            return nodes
                .get(&current)
                .cloned()
                .map(|node| (current, node))
                .ok_or_else(|| FileReadError::new(FileReadErrorKind::NotFound, "fixture missing"));
        }
        Err(FileReadError::new(
            FileReadErrorKind::Io,
            "fixture link cycle",
        ))
    }
    fn inspect_value(
        &self,
        path: &Path,
        links: LinkPolicy,
        policy: &Policy,
    ) -> Result<FileMetadata, FileReadError> {
        let (physical, node) = self.resolve(path, links == LinkPolicy::Follow)?;
        policy.check_read(&ResolvedRead::new(
            path.into(),
            physical.clone(),
            ReadOrigin::Explicit,
        ))?;
        Ok(FileMetadata {
            lexical_path: path.into(),
            physical_path: physical,
            kind: node.kind,
            hidden: node.hidden,
        })
    }
}
impl AuthorizedReadService for Fake {
    type Security = Policy;
    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        policy: &'a Policy,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async move {
            self.record("inspect", path, links, policy, cancel, None)?;
            self.inspect_value(path, links, policy)
                .map_err(OperationError::Failed)
        })
    }
    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        policy: &'a Policy,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async move {
            self.record("list", path, links, policy, cancel, None)?;
            let parent = self
                .inspect_value(path, links, policy)
                .map_err(OperationError::Failed)?;
            if parent.kind != EntryKind::Directory {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Io,
                    "not a directory",
                )));
            }
            let nodes = self.nodes.lock().unwrap();
            let mut result: Vec<_> = nodes
                .iter()
                .filter(|(path, _)| path.parent() == Some(parent.physical_path.as_path()))
                .map(|(physical, node)| FileMetadata {
                    lexical_path: path.join(physical.file_name().unwrap()),
                    physical_path: physical.clone(),
                    kind: node.kind,
                    hidden: node.hidden,
                })
                .collect();
            result.reverse(); // The production algorithm must supply source ordinal sorting.
            Ok(result)
        })
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        policy: &'a Policy,
        cancel: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            self.record(
                "read",
                &request.requested,
                request.links,
                policy,
                cancel,
                Some(request.clone()),
            )?;
            let metadata = self
                .inspect_value(&request.requested, request.links, policy)
                .map_err(OperationError::Failed)?;
            if metadata.kind != EntryKind::Regular {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::InvalidOperation,
                    "not regular",
                )));
            }
            if request
                .expected_physical
                .as_ref()
                .is_some_and(|expected| expected != &metadata.physical_path)
            {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TargetChanged,
                    "fixture opened target changed",
                )));
            }
            let bytes = self.resolve(&request.requested, true).unwrap().1.bytes;
            if request.max_bytes.is_some_and(|max| bytes.len() > max) {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TooLarge,
                    "fixture exceeds 1 MiB limit",
                )));
            }
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: metadata.physical_path,
                bytes,
            })
        })
    }
}
fn context() -> SkillDiscoveryContext {
    SkillDiscoveryContext {
        working_directory: "/work".into(),
        platform: PathPlatform::Unix,
    }
}
fn root(path: &str, scope: SkillScope, follow: bool) -> SkillRoot {
    SkillRoot {
        path: path.into(),
        scope,
        follow_directory_links: follow,
    }
}
async fn discover(fake: &Fake, roots: &[SkillRoot]) -> SkillSnapshot {
    discover_skills(
        fake,
        roots,
        &SkillConfiguration::default(),
        &context(),
        &policy(),
        &Cancel::no(),
    )
    .await
    .unwrap()
}

#[test]
fn roots_preserve_nearest_repository_then_user_then_packaged_and_physical_launch_fallback() {
    let workspace = ProjectWorkspace::from_resolved(
        "/repo/src/nested".into(),
        "/repo/src/nested".into(),
        Some("/repo".into()),
        None,
    );
    let roots = resolve_skill_roots(
        &workspace,
        Path::new("/home/user"),
        Path::new("/package"),
        PathPlatform::Unix,
    )
    .unwrap();
    assert_eq!(
        roots
            .iter()
            .map(|root| root.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/repo/src/nested/.agents/skills",
            "/repo/src/.agents/skills",
            "/repo/.agents/skills",
            "/home/user/.agents/skills",
            "/package"
        ]
    );
    assert_eq!(
        roots.iter().map(|root| root.scope).collect::<Vec<_>>(),
        [
            SkillScope::Repo,
            SkillScope::Repo,
            SkillScope::Repo,
            SkillScope::User,
            SkillScope::System
        ]
    );
    assert_eq!(
        roots
            .iter()
            .map(|root| root.follow_directory_links)
            .collect::<Vec<_>>(),
        [true, true, true, true, false]
    );
    let alias = ProjectWorkspace::from_resolved(
        "/outside/alias".into(),
        "/repo/nested".into(),
        Some("/repo".into()),
        None,
    );
    assert_eq!(
        resolve_skill_roots(
            &alias,
            Path::new(""),
            Path::new("/package"),
            PathPlatform::Unix
        )
        .unwrap()
        .iter()
        .map(|root| root.path.as_str())
        .collect::<Vec<_>>(),
        [
            "/repo/nested/.agents/skills",
            "/repo/.agents/skills",
            "/package"
        ]
    );
    let no_repo = ProjectWorkspace::from_resolved("/work/a".into(), "/work/a".into(), None, None);
    assert_eq!(
        resolve_skill_roots(
            &no_repo,
            Path::new("/work"),
            Path::new("/work/.agents/skills/"),
            PathPlatform::Unix
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn root_containment_normalizes_macos_aliases_but_preserves_case_and_lexical_root_spelling() {
    let workspace = ProjectWorkspace::from_resolved(
        "/tmp/repo/child".into(),
        "/private/tmp/repo/child".into(),
        Some("/private/tmp/repo".into()),
        None,
    );
    let roots = resolve_skill_roots(
        &workspace,
        Path::new(""),
        Path::new("/package"),
        PathPlatform::MacOs,
    )
    .unwrap();
    assert_eq!(roots[0].path, "/tmp/repo/child/.agents/skills");
    assert_eq!(roots[1].path, "/tmp/repo/.agents/skills");
    let different_case = ProjectWorkspace::from_resolved(
        "/Tmp/repo/child".into(),
        "/other".into(),
        Some("/tmp/repo".into()),
        None,
    );
    assert_eq!(
        resolve_skill_roots(
            &different_case,
            Path::new(""),
            Path::new("/package"),
            PathPlatform::MacOs
        )
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn discovery_bfs_order_duplicates_disabled_and_optional_display_metadata_match_source() {
    let fake = Fake::default();
    fake.dir("/first");
    fake.dir("/second");
    fake.skill("/first/z/SKILL.md", "same");
    fake.skill("/first/a/SKILL.md", "same");
    fake.skill("/second/a/SKILL.md", "same");
    fake.file("/first/a/agents/openai.yaml", "interface:\n  display_name: Display\n  short_description: OpenAI short\npolicy:\n  allow_implicit_invocation: false\n");
    let configuration = SkillConfiguration::new(
        true,
        vec![SkillConfigEntry::new("/first/z/SKILL.md", false)],
    );
    let result = discover_skills(
        &fake,
        &[
            root("/first", SkillScope::Repo, true),
            root("/second", SkillScope::User, true),
        ],
        &configuration,
        &context(),
        &policy(),
        &Cancel::no(),
    )
    .await
    .unwrap();
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/first/a/SKILL.md",
            "/first/z/SKILL.md",
            "/second/a/SKILL.md"
        ]
    );
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.enabled)
            .collect::<Vec<_>>(),
        [true, false, true]
    );
    assert_eq!(result.skills[0].display_name.as_deref(), Some("Display"));
    assert_eq!(
        result.skills[0].short_description.as_deref(),
        Some("OpenAI short")
    );
    assert!(!result.skills[0].prompt_visible);
    assert!(result.errors.is_empty());
    let calls = fake.calls.lock().unwrap();
    assert!(calls.iter().all(|call| call.policy == 41));
    assert_eq!(calls[0].links, LinkPolicy::NoFollow);
    assert_eq!(calls[1].links, LinkPolicy::Follow);
    for call in calls.iter().filter(|call| call.operation == "read") {
        let request = call.request.as_ref().unwrap();
        assert!(request.expected_physical.is_some());
        assert_eq!(request.max_bytes, Some(1024 * 1024));
        assert_eq!(request.links, LinkPolicy::Follow);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn frontmatter_short_description_and_invocation_override_sidecar_defaults() {
    for (flag, expected) in [("", false), ("disable-model-invocation: false\n", true)] {
        let fake = Fake::default();
        fake.dir("/skills");
        fake.dir("/skills/a");
        fake.file("/skills/a/SKILL.md", format!("---\nname: a\ndescription: description\n{flag}metadata:\n  short-description: Frontmatter short\n---\nbody"));
        fake.file("/skills/a/agents/openai.yaml", "interface:\n  short_description: Sidecar short\npolicy:\n  allow_implicit_invocation: false\n");
        let result = discover(&fake, &[root("/skills", SkillScope::User, true)]).await;
        assert_eq!(
            result.skills[0].short_description.as_deref(),
            Some("Frontmatter short")
        );
        assert_eq!(result.skills[0].prompt_visible, expected);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn discovery_enforces_exact_name_depth_hidden_and_isolates_bad_frontmatter() {
    let fake = Fake::default();
    fake.dir("/skills");
    fake.skill("/skills/valid/SKILL.md", "valid");
    fake.skill("/skills/.hidden/SKILL.md", "hidden");
    fake.skill("/skills/attribute-hidden/SKILL.md", "attribute-hidden");
    fake.nodes
        .lock()
        .unwrap()
        .get_mut(Path::new("/skills/attribute-hidden"))
        .unwrap()
        .hidden = true;
    fake.dir("/skills/bad");
    fake.file("/skills/bad/SKILL.md", "not frontmatter");
    fake.dir("/skills/lower");
    fake.file("/skills/lower/skill.md", "not loaded");
    let mut current = "/skills".to_owned();
    for depth in 1..=7 {
        current.push_str(&format!("/d{depth}"));
        fake.skill(&format!("{current}/SKILL.md"), &format!("depth-{depth}"));
    }
    let result = discover(&fake, &[root("/skills", SkillScope::User, true)]).await;
    assert!(result.skills.iter().any(|skill| skill.name == "valid"));
    assert!(result.skills.iter().any(|skill| skill.name == "depth-6"));
    assert!(!result.skills.iter().any(|skill| {
        ["hidden", "attribute-hidden", "depth-7", "lower"].contains(&skill.name.as_str())
    }));
    assert_eq!(result.errors.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn directory_links_are_followed_only_when_allowed_cycles_dedup_and_file_links_skipped() {
    let fake = Fake::default();
    fake.dir("/skills");
    fake.dir("/target");
    fake.skill("/target/a/SKILL.md", "linked");
    fake.link("/skills/alias", "/target");
    fake.link("/target/cycle", "/skills");
    fake.link("/skills/SKILL.md", "/target/a/SKILL.md");
    let result = discover(
        &fake,
        &[
            root("/skills", SkillScope::User, true),
            root("/target", SkillScope::Repo, true),
        ],
    )
    .await;
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].path, "/target/a/SKILL.md");
    assert_eq!(result.skills[0].discovery_path, "/skills/alias/a/SKILL.md");
    assert_eq!(result.skills[0].scope, SkillScope::User);
    assert!(result.errors.is_empty());
    let system = discover(&fake, &[root("/skills", SkillScope::System, false)]).await;
    assert!(system.skills.is_empty());
    fake.link("/system-link", "/skills");
    let linked = discover(&fake, &[root("/system-link", SkillScope::System, false)]).await;
    assert_eq!(
        linked.errors[0].message,
        "System skill roots cannot be symbolic links."
    );
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.path == Path::new("/system-link") && call.links == LinkPolicy::Follow)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn entry_sort_is_utf16_ordinal_not_unicode_scalar_order() {
    let fake = Fake::default();
    fake.dir("/skills");
    fake.skill("/skills/\u{E000}/SKILL.md", "bmp");
    fake.skill("/skills/\u{10000}/SKILL.md", "supplementary");
    let result = discover(&fake, &[root("/skills", SkillScope::User, true)]).await;
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["supplementary", "bmp"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn discovery_enforces_per_root_directory_and_entry_limits() {
    let fake = Fake::default();
    fake.dir("/directories");
    fake.dir("/entries");
    for index in 0..2001 {
        fake.dir(&format!("/directories/d{index:04}"));
    }
    for index in 0..20001 {
        fake.node(
            &format!("/entries/e{index:05}"),
            EntryKind::Other,
            vec![],
            None,
            false,
        );
    }
    let result = discover(
        &fake,
        &[
            root("/directories", SkillScope::User, true),
            root("/entries", SkillScope::User, true),
        ],
    )
    .await;
    assert_eq!(
        result
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>(),
        [
            "Skill discovery exceeded 2000 directories.",
            "Skill discovery exceeded 20000 entries."
        ]
    );
    let calls = fake.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.operation == "list" && call.path.starts_with("/directories"))
            .count(),
        2000
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.operation == "inspect" && call.path == Path::new("/entries/e20000"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn errors_isolate_io_invalid_utf8_oversize_and_optional_metadata_not_unexpected_categories() {
    let fake = Fake::default();
    fake.dir("/skills");
    fake.dir("/skills/invalid");
    fake.file("/skills/invalid/SKILL.md", vec![0xff]);
    fake.dir("/skills/large");
    fake.file("/skills/large/SKILL.md", vec![0; 1024 * 1024 + 1]);
    fake.skill("/skills/good/SKILL.md", "good");
    fake.file("/skills/good/agents/openai.yaml", vec![0xff]);
    fake.skill("/skills/denied/SKILL.md", "denied");
    fake.error(
        "read",
        "/skills/denied/SKILL.md",
        FileReadErrorKind::Denied,
        "  unavailable\n\t fixture  ",
    );
    let result = discover(
        &fake,
        &[
            root("/missing", SkillScope::User, true),
            root("/skills", SkillScope::User, true),
        ],
    )
    .await;
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "good");
    assert_eq!(result.errors.len(), 4);
    assert!(
        result
            .errors
            .iter()
            .any(|error| error.message.contains("UTF-8"))
    );
    assert!(
        result
            .errors
            .iter()
            .any(|error| error.message.contains("1 MiB"))
    );
    assert!(
        result
            .errors
            .iter()
            .any(|error| error.message == "unavailable fixture")
    );
    fake.error(
        "read",
        "/skills/good/SKILL.md",
        FileReadErrorKind::InvalidPath,
        "invalid path",
    );
    assert!(
        matches!(discover_skills(&fake, &[root("/skills", SkillScope::User, true)], &SkillConfiguration::default(), &context(), &policy(), &Cancel::no()).await, Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::InvalidPath)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn changed_target_is_diagnostic_and_never_reopened_without_expected_identity() {
    let fake = Fake::default();
    fake.dir("/skills");
    fake.dir("/skills/a");
    fake.link("/skills/a/SKILL.md", "/actual/SKILL.md");
    fake.skill("/actual/SKILL.md", "actual");
    // A raced entry is initially observed regular but has changed before open.
    let request = ReadRequest {
        requested: "/skills/a/SKILL.md".into(),
        expected_physical: Some("/skills/a/SKILL.md".into()),
        links: LinkPolicy::Follow,
        max_bytes: Some(1024 * 1024),
    };
    let error = read_skill_file(
        &fake,
        &request.requested,
        request.expected_physical.as_ref().unwrap(),
        &policy(),
        &Cancel::no(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, OperationError::Failed(error) if error.kind == FileReadErrorKind::TargetChanged)
    );
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.operation == "read")
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_denial_propagate_without_fallback_or_caching_partial_discovery() {
    let fake = Arc::new(Fake::default());
    fake.dir("/skills");
    fake.skill("/skills/a/SKILL.md", "a");
    let cancelled = Cancel(AtomicBool::new(true));
    assert!(matches!(
        discover_skills(
            fake.as_ref(),
            &[root("/skills", SkillScope::User, true)],
            &SkillConfiguration::default(),
            &context(),
            &policy(),
            &cancelled
        )
        .await,
        Err(OperationError::Cancelled)
    ));
    assert!(fake.calls.lock().unwrap().is_empty());
    *fake.cancel_at.lock().unwrap() = Some(("read", "/skills/a/SKILL.md".into()));
    assert!(matches!(
        discover_skills(
            fake.as_ref(),
            &[root("/skills", SkillScope::User, true)],
            &SkillConfiguration::default(),
            &context(),
            &policy(),
            &Cancel::no()
        )
        .await,
        Err(OperationError::Cancelled)
    ));
    *fake.cancel_at.lock().unwrap() = None;
    let denied = Policy {
        id: 41,
        denied: Some("/skills".into()),
    };
    let result = discover_skills(
        fake.as_ref(),
        &[root("/skills", SkillScope::User, true)],
        &SkillConfiguration::default(),
        &context(),
        &denied,
        &Cancel::no(),
    )
    .await
    .unwrap();
    assert!(result.skills.is_empty());
    assert_eq!(result.errors[0].message, "fixture denied");
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.operation == "read")
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn diagnostics_are_normalized_utf16_bounded_and_never_hide_empty_errors() {
    let fake = Fake::default();
    fake.dir("/a");
    fake.dir("/b");
    fake.error(
        "list",
        "/a",
        FileReadErrorKind::Io,
        &format!(" \n{} \t suffix", "x".repeat(1100)),
    );
    fake.error("list", "/b", FileReadErrorKind::Io, " \r\n\t ");
    let result = discover(
        &fake,
        &[
            root("/a", SkillScope::User, true),
            root("/b", SkillScope::User, true),
        ],
    )
    .await;
    assert_eq!(result.errors[0].message, "x".repeat(1024));
    assert_eq!(result.errors[1].message, "Skill discovery failed.");
}

#[tokio::test(flavor = "current_thread")]
async fn catalog_cache_keys_manual_and_configuration_generations_not_changed_values_alone() {
    let fake = Arc::new(Fake::default());
    fake.dir("/skills");
    fake.skill("/skills/a/SKILL.md", "a");
    let configuration = Arc::new(Mutex::new((SkillConfiguration::default(), 0)));
    let source = configuration.clone();
    let catalog = SkillCatalog::new(
        fake.clone(),
        vec![root("/skills", SkillScope::User, true)],
        context(),
        Arc::new(move || source.lock().unwrap().clone()),
    );
    let first = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    configuration.lock().unwrap().0.enabled = false;
    let same = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    assert!(Arc::ptr_eq(&first, &same));
    assert!(same.skills[0].enabled);
    catalog.invalidate().await;
    let refreshed = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &refreshed));
    assert!(!refreshed.skills[0].enabled);
    configuration.lock().unwrap().0.enabled = true;
    configuration.lock().unwrap().1 += 1;
    let changed = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    assert!(changed.skills[0].enabled);
    assert!(!Arc::ptr_eq(&changed, &refreshed));
    assert!(!Arc::ptr_eq(
        &catalog.refresh(&policy(), &Cancel::no()).await.unwrap(),
        &changed
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn strict_skill_decoder_preserves_bom_and_accepts_exact_byte_limit() {
    let fake = Fake::default();
    fake.file("/bom", vec![0xef, 0xbb, 0xbf, b'x']);
    fake.file("/limit", vec![b'x'; MAXIMUM_SKILL_FILE_BYTES]);
    assert_eq!(
        read_skill_file(
            &fake,
            Path::new("/bom"),
            Path::new("/bom"),
            &policy(),
            &Cancel::no()
        )
        .await
        .unwrap(),
        "\u{feff}x"
    );
    assert_eq!(
        read_skill_file(
            &fake,
            Path::new("/limit"),
            Path::new("/limit"),
            &policy(),
            &Cancel::no()
        )
        .await
        .unwrap()
        .len(),
        MAXIMUM_SKILL_FILE_BYTES
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_resampling_keeps_prior_snapshot_and_never_caches_partial_discovery() {
    let fake = Arc::new(Fake::default());
    fake.dir("/skills");
    fake.skill("/skills/a/SKILL.md", "first");
    let catalog = SkillCatalog::new(
        fake.clone(),
        vec![root("/skills", SkillScope::User, true)],
        context(),
        Arc::new(|| (SkillConfiguration::default(), 0)),
    );
    let first = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    fake.skill("/skills/a/SKILL.md", "second");
    catalog.invalidate().await;
    *fake.cancel_at.lock().unwrap() = Some(("read", "/skills/a/SKILL.md".into()));
    assert!(matches!(
        catalog.capture(&policy(), &Cancel::no()).await,
        Err(OperationError::Cancelled)
    ));
    *fake.cancel_at.lock().unwrap() = None;
    let next = catalog.capture(&policy(), &Cancel::no()).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &next));
    assert_eq!(first.skills[0].name, "first");
    assert_eq!(next.skills[0].name, "second");
}

#[tokio::test(flavor = "current_thread")]
async fn catalog_configure_persists_canonical_path_and_preserves_errors() {
    let fake = Arc::new(Fake::default());
    fake.dir("/skills");
    fake.skill("/skills/a/SKILL.md", "a");
    let configuration = Arc::new(Mutex::new((SkillConfiguration::default(), 0)));
    let source = configuration.clone();
    let target = configuration.clone();
    let catalog = SkillCatalog::new(
        fake.clone(),
        vec![root("/skills", SkillScope::User, true)],
        context(),
        Arc::new(move || source.lock().unwrap().clone()),
    )
    .with_configuration(Arc::new(move |path, enabled| {
        assert_eq!(path, "/skills/a/SKILL.md");
        let mut state = target.lock().unwrap();
        state.0.entries.push(SkillConfigEntry::new(path, enabled));
        state.1 += 1;
        Ok(())
    }));
    assert!(
        !catalog
            .configure("/skills/a/../a/SKILL.md", false, &policy(), &Cancel::no())
            .await
            .unwrap()
            .enabled
    );
    assert!(
        matches!(catalog.configure("/missing", true, &policy(), &Cancel::no()).await, Err(OperationError::Failed(error)) if error.message == "skill path is not present in this session catalog")
    );
    let no_writer = SkillCatalog::new(
        fake.clone(),
        vec![root("/skills", SkillScope::User, true)],
        context(),
        Arc::new(|| (SkillConfiguration::default(), 0)),
    );
    assert!(
        matches!(no_writer.configure("/skills/a/SKILL.md", false, &policy(), &Cancel::no()).await, Err(OperationError::Failed(error)) if error.message == "skill configuration is unavailable")
    );
    let remove = fake.clone();
    let disappearing = no_writer.with_configuration(Arc::new(move |path, _| {
        remove.nodes.lock().unwrap().remove(Path::new(path));
        Ok(())
    }));
    assert!(
        matches!(disappearing.configure("/skills/a/SKILL.md", false, &policy(), &Cancel::no()).await, Err(OperationError::Failed(error)) if error.message == "skill configuration was persisted, but the skill is no longer present in this session catalog")
    );
}
