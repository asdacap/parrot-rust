use parrot_domain::{
    contracts::{
        filesystem::{FileReadError, FileReadErrorKind},
        paths::ResolvedRead,
        security::{AccessSnapshot, RestrictableAccessSnapshot},
    },
    values::{
        config::{PathPlatform, PlatformPath},
        project_workspace::ProjectWorkspace,
    },
};
use parrot_foundation::{
    config::runtime::{SandboxRule, SandboxRuleAction},
    paths::AgentScratchDirectory,
};
use parrot_runtime::plan_mode::{ModeError, ModeResult, RetainedModePolicyMaterializer};
use parrot_tools::session::WriteTargetKind;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

fn invalid(message: impl Into<String>) -> FileReadError {
    FileReadError::new(FileReadErrorKind::InvalidPath, message)
}

fn denied(message: impl Into<String>) -> FileReadError {
    FileReadError::new(FileReadErrorKind::Denied, message)
}

fn normalize(path: &Path) -> Result<PathBuf, FileReadError> {
    if !path.is_absolute() {
        return Err(invalid("Security policy paths must be absolute."));
    }
    let text = path
        .to_str()
        .ok_or_else(|| invalid("Security policy paths must be valid Unicode."))?;
    let platform = if cfg!(target_os = "macos") {
        PathPlatform::MacOs
    } else {
        PathPlatform::Unix
    };
    let normalized =
        PlatformPath::normalize(text, "/", platform).map_err(|error| invalid(error.to_string()))?;
    Ok(PathBuf::from(normalized))
}

fn io_error(error: std::io::Error) -> FileReadError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => FileReadErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => FileReadErrorKind::Denied,
        std::io::ErrorKind::InvalidInput => FileReadErrorKind::InvalidPath,
        _ => FileReadErrorKind::Io,
    };
    FileReadError::new(kind, error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductionAccessRule {
    pub path: PathBuf,
    pub read: bool,
    pub write: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductionWriteTarget {
    path: PathBuf,
    kind: WriteTargetKind,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ProductionWriteTarget {
    pub fn resolve(path: &Path) -> Result<Self, FileReadError> {
        let path = normalize(path)?;
        let physical = fs::canonicalize(&path).map_err(io_error)?;
        let metadata = fs::metadata(&physical).map_err(io_error)?;
        let kind = if metadata.is_dir() {
            WriteTargetKind::Directory
        } else if metadata.is_file() {
            WriteTargetKind::File
        } else {
            return Err(invalid(
                "Permission targets must be regular files or directories.",
            ));
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            path: normalize(&physical)?,
            kind,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    pub fn validate(&self) -> Result<(), FileReadError> {
        let current = Self::resolve(&self.path)
            .map_err(|_| denied("The security write target changed after approval."))?;
        if &current != self {
            return Err(denied("The security write target changed after approval."));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> WriteTargetKind {
        self.kind
    }

    fn contains(&self, other: &Self) -> bool {
        match self.kind {
            WriteTargetKind::Directory => other.path.starts_with(&self.path),
            WriteTargetKind::File => other.kind == WriteTargetKind::File && other.path == self.path,
        }
    }

    fn permits(&self, path: &Path) -> bool {
        self.validate().is_ok()
            && match self.kind {
                WriteTargetKind::Directory => path.starts_with(&self.path),
                WriteTargetKind::File => path == self.path,
            }
    }
}

#[derive(Clone, Copy)]
struct Access {
    read: bool,
    write: bool,
    authorized: bool,
}

impl Access {
    const DENIED: Self = Self {
        read: false,
        write: false,
        authorized: false,
    };
}

#[derive(Clone)]
struct Rule {
    path: PathBuf,
    action: SandboxRuleAction,
}

#[derive(Clone)]
struct AgentContext {
    scratch: AgentScratchDirectory,
    launch_directory: PathBuf,
    queue_directory: PathBuf,
}

#[derive(Clone)]
enum Policy {
    Configured(Vec<Rule>),
    Intersection(ProductionPolicy, ProductionPolicy),
    Agent {
        base: ProductionPolicy,
        writable_roots: Vec<PathBuf>,
        session_root: PathBuf,
        approvals: Vec<ProductionWriteTarget>,
    },
}

/// An immutable policy tree. Approval storage is never retained by a capture.
#[derive(Clone)]
pub struct ProductionPolicy {
    policy: Arc<Policy>,
    read_only: bool,
    context: Option<AgentContext>,
}

impl ProductionPolicy {
    pub fn compose(
        read_only: bool,
        mode_rules: &[SandboxRule],
        global_rules: &[SandboxRule],
        mandatory_rules: &[SandboxRule],
    ) -> Result<Self, FileReadError> {
        let parse = |rule: &SandboxRule| -> Result<Rule, FileReadError> {
            Ok(Rule {
                path: normalize(Path::new(&rule.path))?,
                action: rule.action,
            })
        };
        let mut rules = global_rules
            .iter()
            .chain(mode_rules)
            .map(parse)
            .collect::<Result<Vec<_>, _>>()?;
        rules.sort_by_key(|rule| rule.path.to_string_lossy().encode_utf16().count());
        rules.extend(
            mandatory_rules
                .iter()
                .map(parse)
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(Self {
            policy: Arc::new(Policy::Configured(rules)),
            read_only,
            context: None,
        })
    }

    pub fn allows_read(&self, path: &Path) -> bool {
        normalize(path).is_ok_and(|path| self.evaluate(&path).read)
    }

    pub fn allows_write(&self, path: &Path) -> bool {
        normalize(path).is_ok_and(|path| self.evaluate(&path).write)
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn agent_scratch(&self) -> Option<&AgentScratchDirectory> {
        self.context.as_ref().map(|context| &context.scratch)
    }

    pub fn launch_directory(&self) -> &Path {
        self.context
            .as_ref()
            .map_or(Path::new("/"), |context| context.launch_directory.as_path())
    }

    pub fn queue_directory(&self) -> Option<&Path> {
        self.context
            .as_ref()
            .map(|context| context.queue_directory.as_path())
    }

    pub fn materialized_rules(&self) -> Vec<ProductionAccessRule> {
        let mut paths = BTreeSet::from([PathBuf::from("/")]);
        self.paths(&mut paths);
        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort_by_key(|path| path.to_string_lossy().encode_utf16().count());
        paths
            .into_iter()
            .filter_map(|path| {
                let access = self.evaluate(&path);
                let write = access.write && (self.has_agent_boundary() || access.authorized);
                if path == Path::new("/") && access.read && !write {
                    None
                } else {
                    Some(ProductionAccessRule {
                        path,
                        read: access.read,
                        write,
                    })
                }
            })
            .collect()
    }

    fn for_agent(
        &self,
        workspace: &ProjectWorkspace,
        session_root: &Path,
        scratch: &AgentScratchDirectory,
        approvals: Vec<ProductionWriteTarget>,
    ) -> Result<Self, FileReadError> {
        let writable_roots = if self.read_only {
            Vec::new()
        } else {
            workspace
                .writable_roots()
                .iter()
                .map(|path| normalize(path))
                .collect::<Result<_, _>>()?
        };
        Ok(Self {
            policy: Arc::new(Policy::Agent {
                base: self.clone(),
                writable_roots,
                session_root: session_root.to_path_buf(),
                approvals: if self.read_only {
                    Vec::new()
                } else {
                    approvals
                },
            }),
            read_only: self.read_only,
            context: Some(AgentContext {
                scratch: scratch.clone(),
                launch_directory: normalize(workspace.launch_directory())?,
                queue_directory: session_root.join("queues"),
            }),
        })
    }

    fn evaluate(&self, path: &Path) -> Access {
        match self.policy.as_ref() {
            Policy::Configured(rules) => {
                let mut access = Access {
                    read: true,
                    write: !self.read_only,
                    authorized: false,
                };
                for rule in rules.iter().filter(|rule| path.starts_with(&rule.path)) {
                    access = match rule.action {
                        SandboxRuleAction::AllowWrite => Access {
                            read: true,
                            write: true,
                            authorized: true,
                        },
                        SandboxRuleAction::DenyRead => Access::DENIED,
                        SandboxRuleAction::AllowRead => Access {
                            read: true,
                            ..access
                        },
                        SandboxRuleAction::DenyWrite => Access {
                            write: false,
                            authorized: false,
                            ..access
                        },
                    };
                }
                access
            }
            Policy::Intersection(parent, child) => {
                let parent = parent.evaluate(path);
                let child = child.evaluate(path);
                let write = parent.write && child.write;
                Access {
                    read: parent.read && child.read,
                    write,
                    authorized: write && (parent.authorized || child.authorized),
                }
            }
            Policy::Agent {
                base,
                writable_roots,
                session_root,
                approvals,
            } => {
                let policy = base.evaluate(path);
                if path.starts_with(session_root) {
                    return Access {
                        write: true,
                        authorized: true,
                        ..policy
                    };
                }
                Access {
                    write: policy.write
                        && (policy.authorized
                            || writable_roots.iter().any(|root| path.starts_with(root))
                            || approvals.iter().any(|approval| approval.permits(path))),
                    ..policy
                }
            }
        }
    }

    fn has_agent_boundary(&self) -> bool {
        match self.policy.as_ref() {
            Policy::Configured(_) => false,
            Policy::Agent { .. } => true,
            Policy::Intersection(parent, child) => {
                parent.has_agent_boundary() || child.has_agent_boundary()
            }
        }
    }

    fn paths(&self, paths: &mut BTreeSet<PathBuf>) {
        match self.policy.as_ref() {
            Policy::Configured(rules) => paths.extend(rules.iter().map(|rule| rule.path.clone())),
            Policy::Intersection(parent, child) => {
                parent.paths(paths);
                child.paths(paths);
            }
            Policy::Agent {
                base,
                writable_roots,
                session_root,
                approvals,
            } => {
                base.paths(paths);
                paths.extend(writable_roots.iter().cloned());
                paths.insert(session_root.clone());
                paths.extend(approvals.iter().map(|target| target.path.clone()));
            }
        }
    }
}

impl AccessSnapshot for ProductionPolicy {
    type Error = FileReadError;

    fn check_read(&self, read: &ResolvedRead) -> Result<(), Self::Error> {
        if self.allows_read(read.requested_path()) && self.allows_read(read.actual_path()) {
            Ok(())
        } else {
            Err(denied(format!(
                "Read access denied: {}",
                read.requested_path().display()
            )))
        }
    }

    fn check_write(&self, actual_target: &Path) -> Result<(), Self::Error> {
        if self.allows_write(actual_target) {
            Ok(())
        } else {
            Err(denied(format!(
                "Write access denied: {}",
                actual_target.display()
            )))
        }
    }
}

impl RestrictableAccessSnapshot for ProductionPolicy {
    fn restrict_with(&self, child: &Self) -> Self {
        Self {
            policy: Arc::new(Policy::Intersection(self.clone(), child.clone())),
            read_only: self.read_only || child.read_only,
            context: child.context.clone(),
        }
    }
}

struct SessionContext {
    root: PathBuf,
    workspace: ProjectWorkspace,
}

struct AgentState {
    raw_policy: ProductionPolicy,
    approvals: Vec<ProductionWriteTarget>,
}

#[derive(Default)]
struct MaterializerState {
    sessions: Vec<SessionContext>,
    agents: HashMap<PathBuf, AgentState>,
}

#[derive(Default)]
pub struct ProductionPolicyMaterializer {
    state: Mutex<MaterializerState>,
}

impl ProductionPolicyMaterializer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_session(
        &self,
        root: PathBuf,
        workspace: ProjectWorkspace,
    ) -> Result<(), FileReadError> {
        let root = normalize(&root)?;
        normalize(workspace.launch_directory())?;
        normalize(workspace.physical_identity())?;
        for writable_root in workspace.writable_roots() {
            normalize(writable_root)?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| denied("Policy materializer is unavailable."))?;
        if let Some(existing) = state.sessions.iter().find(|session| session.root == root) {
            if existing.workspace.launch_directory() == workspace.launch_directory()
                && existing.workspace.physical_identity() == workspace.physical_identity()
                && existing.workspace.repository_root() == workspace.repository_root()
                && existing.workspace.writable_roots() == workspace.writable_roots()
            {
                let agents = root.join("root-agents");
                state
                    .agents
                    .retain(|scratch, _| !scratch.starts_with(&agents));
                return Ok(());
            }
            return Err(invalid("The session security workspace is already bound."));
        }
        state.sessions.push(SessionContext { root, workspace });
        Ok(())
    }

    fn resolve_for_agent(
        state: &MaterializerState,
        current: &ProductionPolicy,
        scratch: &AgentScratchDirectory,
    ) -> Result<(ProjectWorkspace, PathBuf, ProductionPolicy), FileReadError> {
        let session = state
            .sessions
            .iter()
            .filter(|session| scratch.root().starts_with(session.root.join("root-agents")))
            .max_by_key(|session| session.root.components().count())
            .ok_or_else(|| {
                denied("The agent scratch directory is outside registered session resources.")
            })?;
        let raw = match current.policy.as_ref() {
            Policy::Agent {
                base, session_root, ..
            } if session_root == &session.root
                && current
                    .agent_scratch()
                    .is_some_and(|selected| selected.root() == scratch.root()) =>
            {
                base.clone()
            }
            _ => current.clone(),
        };
        Ok((session.workspace.clone(), session.root.clone(), raw))
    }

    pub fn approve_for_agent(
        &self,
        current: &Arc<ProductionPolicy>,
        scratch: &AgentScratchDirectory,
        targets: &[ProductionWriteTarget],
    ) -> Result<Arc<ProductionPolicy>, FileReadError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| denied("Policy materializer is unavailable."))?;
        let (workspace, root, raw) = Self::resolve_for_agent(&state, current, scratch)?;
        if current.is_read_only() {
            return Err(denied("The current security policy is read-only."));
        }
        if current.agent_scratch() != Some(scratch) {
            return Err(denied(
                "The current security policy belongs to a different agent.",
            ));
        }
        let agent = state
            .agents
            .get(scratch.root())
            .ok_or_else(|| denied("The agent policy has not been materialized."))?;
        for target in targets {
            target.validate()?;
        }
        let mut approvals = agent
            .approvals
            .iter()
            .filter(|target| target.validate().is_ok())
            .cloned()
            .collect::<Vec<_>>();
        for target in targets {
            if !approvals.iter().any(|existing| existing.contains(target)) {
                approvals.retain(|existing| !target.contains(existing));
                approvals.push(target.clone());
            }
        }
        let snapshot = Arc::new(raw.for_agent(&workspace, &root, scratch, approvals.clone())?);
        state.agents.insert(
            scratch.root().to_path_buf(),
            AgentState {
                raw_policy: raw,
                approvals,
            },
        );
        Ok(snapshot)
    }

    pub fn approve(
        &self,
        scratch: &AgentScratchDirectory,
        targets: &[ProductionWriteTarget],
    ) -> Result<(), FileReadError> {
        for target in targets {
            target.validate()?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| denied("Policy materializer is unavailable."))?;
        let agent = state
            .agents
            .get_mut(scratch.root())
            .ok_or_else(|| denied("The agent policy has not been materialized."))?;
        if agent.raw_policy.is_read_only() {
            return Err(denied("The current security policy is read-only."));
        }
        for target in targets {
            target.validate()?;
        }
        agent.approvals.retain(|target| target.validate().is_ok());
        for target in targets {
            if !agent
                .approvals
                .iter()
                .any(|existing| existing.contains(target))
            {
                agent
                    .approvals
                    .retain(|existing| !target.contains(existing));
                agent.approvals.push(target.clone());
            }
        }
        Ok(())
    }
}

impl RetainedModePolicyMaterializer<ProductionPolicy> for ProductionPolicyMaterializer {
    fn for_agent(
        &self,
        raw_profile: &Arc<ProductionPolicy>,
        selected_scratch: &AgentScratchDirectory,
    ) -> ModeResult<Arc<ProductionPolicy>> {
        let capture = || -> Result<ProductionPolicy, FileReadError> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| denied("Policy materializer is unavailable."))?;
            let (workspace, root, raw) =
                Self::resolve_for_agent(&state, raw_profile, selected_scratch)?;
            let agent = state
                .agents
                .entry(selected_scratch.root().to_path_buf())
                .or_insert_with(|| AgentState {
                    raw_policy: raw.clone(),
                    approvals: Vec::new(),
                });
            agent.raw_policy = raw.clone();
            agent.approvals.retain(|target| target.validate().is_ok());
            raw.for_agent(&workspace, &root, selected_scratch, agent.approvals.clone())
        };
        capture()
            .map(Arc::new)
            .map_err(|error| ModeError::InvalidArgument(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::contracts::paths::ReadOrigin;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        workspace: ProjectWorkspace,
        session: PathBuf,
        scratch: AgentScratchDirectory,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "parrot-policy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let work = root.join("workspace");
            let session = root.join("session");
            fs::create_dir_all(&work).unwrap();
            let scratch =
                AgentScratchDirectory::new(&session.join("root-agents/main"), &root).unwrap();
            let workspace = ProjectWorkspace::from_resolved(work.clone(), work, None, None);
            Self {
                root,
                workspace,
                session,
                scratch,
            }
        }
        fn materializer(&self) -> ProductionPolicyMaterializer {
            let materializer = ProductionPolicyMaterializer::new();
            materializer
                .register_session(self.session.clone(), self.workspace.clone())
                .unwrap();
            materializer
        }
        fn capture(
            &self,
            materializer: &ProductionPolicyMaterializer,
            raw: &ProductionPolicy,
        ) -> Arc<ProductionPolicy> {
            materializer
                .for_agent(&Arc::new(raw.clone()), &self.scratch)
                .unwrap()
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
    fn raw(read_only: bool) -> ProductionPolicy {
        ProductionPolicy::compose(read_only, &[], &[], &[]).unwrap()
    }

    #[test]
    fn configured_order_and_mandatory_rules_retain_exceptions() {
        let root = Path::new("/policy");
        let profile = ProductionPolicy::compose(
            false,
            &[rule(&root.join("allowed"), SandboxRuleAction::AllowWrite)],
            &[rule(root, SandboxRuleAction::DenyRead)],
            &[rule(
                &root.join("allowed/secret"),
                SandboxRuleAction::DenyRead,
            )],
        )
        .unwrap();
        assert!(!profile.allows_read(root));
        assert!(profile.allows_write(&root.join("allowed/file")));
        assert!(!profile.allows_read(&root.join("allowed/secret/file")));
        assert!(profile.allows_read(Path::new("/policy-other")));
    }

    #[test]
    fn same_path_mode_rule_overrides_global_and_relative_rules_fail() {
        let path = Path::new("/policy");
        let profile = ProductionPolicy::compose(
            false,
            &[rule(path, SandboxRuleAction::AllowWrite)],
            &[rule(path, SandboxRuleAction::DenyRead)],
            &[],
        )
        .unwrap();
        assert!(profile.allows_write(path));
        assert!(
            ProductionPolicy::compose(
                false,
                &[rule(Path::new("relative"), SandboxRuleAction::AllowWrite)],
                &[],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn agent_workspace_and_session_boundary_preserve_configured_exceptions() {
        let fixture = Fixture::new();
        let exception = fixture.root.join("exception");
        let raw = ProductionPolicy::compose(
            false,
            &[],
            &[rule(&exception, SandboxRuleAction::AllowWrite)],
            &[],
        )
        .unwrap();
        let policy = fixture.capture(&fixture.materializer(), &raw);
        assert!(policy.allows_write(&fixture.workspace.launch_directory().join("new")));
        assert!(policy.allows_write(&fixture.session.join("history.jsonl")));
        assert!(policy.allows_write(&exception.join("new")));
        assert!(!policy.allows_write(&fixture.root.join("outside")));
        assert_eq!(policy.agent_scratch(), Some(&fixture.scratch));
        assert_eq!(
            policy.launch_directory(),
            fixture.workspace.launch_directory()
        );
        assert_eq!(
            policy.queue_directory(),
            Some(fixture.session.join("queues").as_path())
        );
    }

    #[test]
    fn read_only_restricts_workspace_but_keeps_owned_session_resources() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let policy = fixture.capture(&materializer, &raw(true));
        assert!(!policy.allows_write(fixture.workspace.launch_directory()));
        assert!(policy.allows_write(&fixture.session.join("scratch")));
        fs::write(fixture.root.join("outside"), b"x").unwrap();
        assert!(
            materializer
                .approve(
                    &fixture.scratch,
                    &[ProductionWriteTarget::resolve(&fixture.root.join("outside")).unwrap()]
                )
                .is_err()
        );
    }

    #[test]
    fn approvals_do_not_mutate_old_capture_and_files_never_grant_subtrees() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let before = fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        materializer
            .approve(
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        let after = fixture.capture(&materializer, &raw(false));
        assert!(!before.allows_write(&path));
        assert!(after.allows_write(&path));
        assert!(!after.allows_write(&path.join("child")));
    }

    #[test]
    fn atomic_read_only_approval_failure_does_not_retain_grants() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let before = fixture.capture(&materializer, &raw(false));
        let current = Arc::new(raw(true).restrict_with(&before));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        let target = ProductionWriteTarget::resolve(&path).unwrap();
        assert!(
            materializer
                .approve_for_agent(&current, &fixture.scratch, &[target])
                .is_err()
        );
        assert!(
            !fixture
                .capture(&materializer, &raw(false))
                .allows_write(&path)
        );
    }

    #[test]
    fn atomic_validation_failure_does_not_retain_any_candidate_grant() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let current = fixture.capture(&materializer, &raw(false));
        let valid_path = fixture.root.join("valid-file");
        let invalid_path = fixture.root.join("replaced-file");
        fs::write(&valid_path, b"x").unwrap();
        fs::write(&invalid_path, b"x").unwrap();
        let valid_target = ProductionWriteTarget::resolve(&valid_path).unwrap();
        let invalid_target = ProductionWriteTarget::resolve(&invalid_path).unwrap();
        fs::rename(&invalid_path, fixture.root.join("old-file")).unwrap();
        fs::write(&invalid_path, b"y").unwrap();
        assert!(invalid_target.validate().is_err());
        assert!(
            materializer
                .approve_for_agent(&current, &fixture.scratch, &[valid_target, invalid_target])
                .is_err()
        );
        let after = fixture.capture(&materializer, &raw(false));
        assert!(!after.allows_write(&valid_path));
        assert!(!after.allows_write(&invalid_path));
    }

    #[test]
    fn atomic_approval_updates_future_captures_without_mutating_old_capture() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let before = fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        let after = materializer
            .approve_for_agent(
                &before,
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        assert!(!before.allows_write(&path));
        assert!(after.allows_write(&path));
        assert!(!after.allows_write(&path.join("child")));
        assert!(
            fixture
                .capture(&materializer, &raw(false))
                .allows_write(&path)
        );
        assert!(
            materializer
                .for_agent(&before, &fixture.scratch)
                .unwrap()
                .allows_write(&path)
        );
        materializer
            .register_session(fixture.session.clone(), fixture.workspace.clone())
            .unwrap();
        assert!(
            materializer
                .approve_for_agent(&after, &fixture.scratch, &[])
                .is_err()
        );
        assert!(
            materializer
                .approve_for_agent(&Arc::new(raw(false)), &fixture.scratch, &[])
                .is_err()
        );
    }

    #[test]
    fn atomic_approval_retains_intersection_and_ancestor_boundaries() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let parent = fixture.capture(&materializer, &raw(false));
        let selected_child = Arc::new(parent.restrict_with(&raw(false)));
        let child_scratch = AgentScratchDirectory::new(
            &fixture.session.join("root-agents/main/child"),
            &fixture.root,
        )
        .unwrap();
        let child = materializer
            .for_agent(&selected_child, &child_scratch)
            .unwrap();
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        let target = ProductionWriteTarget::resolve(&path).unwrap();
        assert!(
            materializer
                .approve_for_agent(&parent, &child_scratch, &[target.clone()])
                .is_err()
        );
        let after = materializer
            .approve_for_agent(&child, &child_scratch, &[target])
            .unwrap();
        assert_eq!(after.agent_scratch(), Some(&child_scratch));
        assert!(!after.allows_write(&path));
        assert!(
            !materializer
                .for_agent(&after, &child_scratch)
                .unwrap()
                .allows_write(&path)
        );
    }

    #[test]
    fn directory_approval_includes_children_but_not_siblings() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-dir");
        fs::create_dir(&path).unwrap();
        materializer
            .approve(
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        let policy = fixture.capture(&materializer, &raw(false));
        assert!(policy.allows_write(&path.join("child")));
        assert!(!policy.allows_write(&fixture.root.join("approved-dir-other")));
    }

    #[test]
    fn replacing_approval_revokes_old_and_future_captures() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        let target = ProductionWriteTarget::resolve(&path).unwrap();
        materializer
            .approve(&fixture.scratch, &[target.clone()])
            .unwrap();
        let old = fixture.capture(&materializer, &raw(false));
        fs::rename(&path, fixture.root.join("old-file")).unwrap();
        fs::write(&path, b"y").unwrap();
        assert!(target.validate().is_err());
        assert!(!old.allows_write(&path));
        assert!(
            !fixture
                .capture(&materializer, &raw(false))
                .allows_write(&path)
        );
    }

    #[test]
    fn intersection_never_widens_and_keeps_child_resource_context() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let child = fixture.capture(&materializer, &raw(false));
        let parent = ProductionPolicy::compose(
            true,
            &[],
            &[rule(
                fixture.workspace.launch_directory(),
                SandboxRuleAction::DenyRead,
            )],
            &[],
        )
        .unwrap();
        let restricted = parent.restrict_with(&child);
        assert!(restricted.is_read_only());
        assert!(!restricted.allows_read(fixture.workspace.launch_directory()));
        assert!(!restricted.allows_write(fixture.workspace.launch_directory()));
        assert_eq!(restricted.agent_scratch(), child.agent_scratch());
        assert_eq!(restricted.launch_directory(), child.launch_directory());
    }

    #[test]
    fn check_read_requires_both_lexical_and_actual_permission() {
        let profile = ProductionPolicy::compose(
            false,
            &[],
            &[rule(Path::new("/secret"), SandboxRuleAction::DenyRead)],
            &[],
        )
        .unwrap();
        for (lexical, actual) in [
            ("/safe/link", "/secret/file"),
            ("/secret/link", "/safe/file"),
        ] {
            assert!(
                profile
                    .check_read(&ResolvedRead::new(
                        lexical.into(),
                        actual.into(),
                        ReadOrigin::Explicit
                    ))
                    .is_err()
            );
        }
    }

    #[test]
    fn sandbox_materialization_starts_read_only_and_applies_effective_boundaries() {
        let raw = raw(false);
        assert!(raw.materialized_rules().is_empty());
        let fixture = Fixture::new();
        let captured = fixture.capture(&fixture.materializer(), &raw);
        let rules = captured.materialized_rules();
        assert!(
            rules
                .iter()
                .any(|rule| rule.path == fixture.workspace.launch_directory() && rule.write)
        );
        assert!(
            rules
                .iter()
                .any(|rule| rule.path == fixture.session && rule.write)
        );
        let denied = ProductionPolicy::compose(
            false,
            &[],
            &[rule(
                fixture.workspace.launch_directory(),
                SandboxRuleAction::DenyWrite,
            )],
            &[],
        )
        .unwrap();
        let restricted = captured.restrict_with(&denied).materialized_rules();
        assert!(
            restricted
                .iter()
                .any(|rule| rule.path == fixture.workspace.launch_directory() && !rule.write)
        );
    }

    #[test]
    fn session_registration_is_idempotent_but_rejects_rebinding_or_foreign_scratch() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        materializer
            .register_session(fixture.session.clone(), fixture.workspace.clone())
            .unwrap();
        let different =
            ProjectWorkspace::from_resolved(fixture.root.clone(), fixture.root.clone(), None, None);
        assert!(
            materializer
                .register_session(fixture.session.clone(), different)
                .is_err()
        );
        let foreign = AgentScratchDirectory::new(
            &fixture.root.join("foreign/root-agents/main"),
            &fixture.root,
        )
        .unwrap();
        assert!(
            materializer
                .for_agent(&Arc::new(raw(false)), &foreign)
                .is_err()
        );
    }

    #[test]
    fn materializing_child_intersection_retains_parent_boundary_and_uses_child_scratch() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let blocked = fixture.workspace.launch_directory().join("blocked");
        let parent_raw = ProductionPolicy::compose(
            false,
            &[],
            &[rule(&blocked, SandboxRuleAction::DenyWrite)],
            &[],
        )
        .unwrap();
        let parent = fixture.capture(&materializer, &parent_raw);
        let selected_child = Arc::new(parent.restrict_with(&raw(false)));
        let child_scratch = AgentScratchDirectory::new(
            &fixture.session.join("root-agents/main/child"),
            &fixture.root,
        )
        .unwrap();
        let child = materializer
            .for_agent(&selected_child, &child_scratch)
            .unwrap();
        assert_eq!(child.agent_scratch(), Some(&child_scratch));
        assert_ne!(child.agent_scratch(), parent.agent_scratch());
        assert_eq!(
            child.launch_directory(),
            fixture.workspace.launch_directory()
        );
        assert!(child.allows_write(&fixture.workspace.launch_directory().join("allowed")));
        assert!(!child.allows_write(&blocked));
        assert!(!child.allows_write(&fixture.root.join("outside")));
    }

    #[test]
    fn refreshing_own_capture_picks_up_grants_without_unwrapping_restrictions() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        let before = fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        materializer
            .approve(
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        let refreshed = materializer.for_agent(&before, &fixture.scratch).unwrap();
        assert!(!before.allows_write(&path));
        assert!(refreshed.allows_write(&path));
        let restricted = Arc::new(raw(true).restrict_with(&before));
        let restricted = materializer
            .for_agent(&restricted, &fixture.scratch)
            .unwrap();
        assert!(!restricted.allows_write(&path));
        assert!(restricted.is_read_only());
    }

    #[test]
    fn new_session_registration_clears_future_grants_without_mutating_old_capture() {
        let fixture = Fixture::new();
        let materializer = fixture.materializer();
        fixture.capture(&materializer, &raw(false));
        let path = fixture.root.join("approved-file");
        fs::write(&path, b"x").unwrap();
        materializer
            .approve(
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        let old = fixture.capture(&materializer, &raw(false));
        materializer
            .register_session(fixture.session.clone(), fixture.workspace.clone())
            .unwrap();
        assert!(old.allows_write(&path));
        assert!(
            !fixture
                .capture(&materializer, &raw(false))
                .allows_write(&path)
        );
    }

    #[test]
    fn configured_denial_cannot_be_bypassed_by_an_approval() {
        let fixture = Fixture::new();
        let path = fixture.root.join("denied");
        fs::write(&path, b"x").unwrap();
        let raw = ProductionPolicy::compose(
            false,
            &[],
            &[rule(&path, SandboxRuleAction::DenyWrite)],
            &[],
        )
        .unwrap();
        let materializer = fixture.materializer();
        fixture.capture(&materializer, &raw);
        materializer
            .approve(
                &fixture.scratch,
                &[ProductionWriteTarget::resolve(&path).unwrap()],
            )
            .unwrap();
        assert!(!fixture.capture(&materializer, &raw).allows_write(&path));
    }
}
