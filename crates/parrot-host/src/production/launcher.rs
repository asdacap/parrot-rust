//! Native process execution bound to one user workspace and an immutable turn capture.

use super::policy::{ProductionAccessRule, ProductionPolicy};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation},
    paths::{ReadOrigin, ResolvedRead},
    security::AccessSnapshot,
};
use parrot_runtime::tool_settlement::SharedSecurity;
use parrot_tools::process::{
    execution::{
        ConfiguredProcessRunner, ProcessResources, ProcessStartRequest, SandboxGate,
        locate_configured, locator::ExecutableLocator,
    },
    ownership::{
        domain::CapturedProcessLauncher,
        execution::{Execution, ProcessLifetime},
    },
    tools::CommandRequest,
};
use std::{io, path::PathBuf, sync::Arc};

/// The user-session constructor supplies these resources once. In particular,
/// command environment overrides cannot select another agent's scratch or queue.
pub struct ProductionLauncher {
    runner: ConfiguredProcessRunner,
    launch_directory: PathBuf,
    queue_directory: PathBuf,
}

impl ProductionLauncher {
    pub fn new(
        gate: Arc<SandboxGate>,
        launch_directory: PathBuf,
        queue_directory: PathBuf,
    ) -> io::Result<Self> {
        Self::with_device_paths(gate, launch_directory, queue_directory, Vec::new())
    }

    pub fn with_device_paths(
        gate: Arc<SandboxGate>,
        launch_directory: PathBuf,
        queue_directory: PathBuf,
        device_paths: Vec<PathBuf>,
    ) -> io::Result<Self> {
        if !launch_directory.is_absolute() || !queue_directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process resource paths must be absolute",
            ));
        }
        let runner = locate_configured(&ExecutableLocator::capture(), gate, device_paths)?;
        Ok(Self {
            runner,
            launch_directory,
            queue_directory,
        })
    }

    pub fn sandbox_available(&self) -> bool {
        self.runner.sandbox_available()
    }
}

impl CapturedProcessLauncher for ProductionLauncher {
    type Request = CommandRequest;
    type Security = SharedSecurity<ProductionPolicy>;

    fn launch(
        &self,
        request: CommandRequest,
        security: Self::Security,
        lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async move {
            lifetime.check().map_err(|_| {
                io::Error::new(io::ErrorKind::Interrupted, "process start was cancelled")
            })?;
            let policy = security.snapshot();
            let scratch = policy.agent_scratch().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "process launch requires an agent-materialized policy",
                )
            })?;
            if policy.launch_directory() != self.launch_directory
                || policy.queue_directory() != Some(self.queue_directory.as_path())
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the captured policy belongs to different process resources",
                ));
            }
            let actual_directory = self.launch_directory.canonicalize()?;
            policy
                .check_read(&ResolvedRead::new(
                    self.launch_directory.clone(),
                    actual_directory,
                    ReadOrigin::Explicit,
                ))
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            let resources = ProcessResources::new(
                self.launch_directory.clone(),
                self.queue_directory.clone(),
                scratch.clone(),
            );
            let mut start = ProcessStartRequest::new(request.command, resources);
            start.environment = request.environment;
            start.terminal_mode = request.terminal_mode;
            start.cancellation = Some(lifetime);

            #[cfg(target_os = "linux")]
            let sandbox_policy = parrot_tools::sandbox::linux::LinuxSandboxPolicy {
                rules: process_rules(policy)?
                    .into_iter()
                    .map(|rule| parrot_tools::sandbox::linux::LinuxAccessRule {
                        path: rule.path,
                        read: rule.read,
                        write: rule.write,
                    })
                    .collect(),
                owned_roots: vec![scratch.root().to_path_buf(), self.queue_directory.clone()],
                device_paths: Vec::new(),
            };
            #[cfg(target_os = "macos")]
            let sandbox_policy = parrot_tools::sandbox::macos::SeatbeltPolicy::compile_rules(
                process_rules(policy)?.into_iter().map(|rule| {
                    (
                        rule.path.to_string_lossy().into_owned(),
                        parrot_tools::sandbox::macos::SeatbeltAccess {
                            read: rule.read,
                            write: rule.write,
                        },
                    )
                }),
            )?;
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let sandbox_policy = ();

            // ProcessRunner alone implements the explicit sandbox gate. A failed
            // sandbox start is returned, never retried outside the sandbox.
            let execution = self.runner.start(&start, &sandbox_policy).await?;
            Ok(Arc::new(execution) as Arc<dyn Execution>)
        })
    }
}

/// Native mounts follow links too. Do not turn a lexical write exception into
/// write access to a different physical target, or reorder conflicting aliases.
fn process_rules(policy: &ProductionPolicy) -> io::Result<Vec<ProductionAccessRule>> {
    let mut rules = Vec::new();
    for rule in policy.materialized_rules() {
        let actual = match rule.path.canonicalize() {
            Ok(actual) => actual,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // The native sandbox retains its source-shaped missing-path
                // behavior; absence never grants access to an ancestor instead.
                rules.push(rule);
                continue;
            }
            Err(error) => return Err(error),
        };
        let read = rule.read && policy.allows_read(&actual);
        let write = rule.write && read && policy.allows_write(&actual);
        if actual != rule.path
            && (rule.read != policy.allows_read(&actual)
                || rule.write != policy.allows_write(&actual))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "sandbox rule resolves to a differently authorized target: {}",
                    rule.path.display()
                ),
            ));
        }
        rules.push(ProductionAccessRule {
            path: rule.path.clone(),
            read,
            write,
        });
        if actual != rule.path {
            rules.push(ProductionAccessRule {
                path: actual,
                read,
                write,
            });
        }
    }
    rules.sort_by_key(|rule| rule.path.components().count());
    rules.dedup_by(|left, right| {
        left.path == right.path && left.read == right.read && left.write == right.write
    });
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::values::project_workspace::ProjectWorkspace;
    use parrot_foundation::{
        config::runtime::{SandboxRule, SandboxRuleAction},
        paths::AgentScratchDirectory,
    };
    use parrot_runtime::plan_mode::RetainedModePolicyMaterializer;
    use parrot_tools::process::execution::TerminalMode;
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        work: PathBuf,
        session: PathBuf,
        scratch: AgentScratchDirectory,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "parrot-production-launcher-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            let work = root.join("work");
            fs::create_dir_all(&work).unwrap();
            let session = root.join("session");
            let scratch =
                AgentScratchDirectory::new(&session.join("root-agents/main"), &root).unwrap();
            Self {
                root,
                work,
                session,
                scratch,
            }
        }
        fn policy(&self, rules: &[SandboxRule]) -> Arc<ProductionPolicy> {
            let materializer = super::super::policy::ProductionPolicyMaterializer::new();
            materializer
                .register_session(
                    self.session.clone(),
                    ProjectWorkspace::from_resolved(
                        self.work.clone(),
                        self.work.clone(),
                        None,
                        None,
                    ),
                )
                .unwrap();
            let raw = Arc::new(ProductionPolicy::compose(false, rules, &[], &[]).unwrap());
            materializer.for_agent(&raw, &self.scratch).unwrap()
        }
        fn launcher(&self, enabled: bool) -> ProductionLauncher {
            ProductionLauncher::new(
                Arc::new(SandboxGate::new(enabled)),
                self.work.clone(),
                self.session.join("queues"),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn command(text: &str) -> CommandRequest {
        CommandRequest {
            command: text.into(),
            environment: vec![],
            terminal_mode: TerminalMode::Pipe,
        }
    }

    #[tokio::test]
    async fn raw_or_foreign_policy_is_rejected_before_launch() {
        let fixture = Fixture::new();
        let launcher = fixture.launcher(false);
        let raw = Arc::new(ProductionPolicy::compose(false, &[], &[], &[]).unwrap());
        let error = launcher
            .launch(
                command("exit 0"),
                SharedSecurity(raw),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let foreign = Fixture::new();
        let error = launcher
            .launch(
                command("exit 0"),
                SharedSecurity(foreign.policy(&[])),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn captured_directory_denial_is_checked_before_process_start() {
        let fixture = Fixture::new();
        let launcher = fixture.launcher(false);
        let policy = fixture.policy(&[SandboxRule {
            path: fixture.work.to_string_lossy().into_owned(),
            action: SandboxRuleAction::DenyRead,
        }]);
        let error = launcher
            .launch(
                command("exit 0"),
                SharedSecurity(policy),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn actual_execution_uses_bound_workspace_and_agent_scratch_not_environment_overrides() {
        let fixture = Fixture::new();
        let launcher = fixture.launcher(false);
        let mut request = command("printf '%s|%s' \"$PWD\" \"$MARKER\"");
        request.environment = vec![
            ("MARKER".into(), "literal-value".into()),
            (
                "AGENT_HISTORY_DIR".into(),
                "/not-an-authorized-resource".into(),
            ),
        ];
        let execution = launcher
            .launch(
                request,
                SharedSecurity(fixture.policy(&[])),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), execution.wait())
            .await
            .unwrap()
            .unwrap();
        execution.settle().await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            result.stdout,
            format!("{}|literal-value", fixture.work.display())
        );
        assert!(fixture.scratch.blob_directory().is_dir());
        assert!(execution.is_completed());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_write_alias_cannot_grant_a_denied_physical_target() {
        let fixture = Fixture::new();
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let alias = fixture.work.join("alias");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        let policy = fixture.policy(&[SandboxRule {
            path: alias.to_string_lossy().into_owned(),
            action: SandboxRuleAction::AllowWrite,
        }]);
        assert!(policy.allows_write(&alias));
        assert!(!policy.allows_write(&outside));
        assert_eq!(
            process_rules(&policy).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn equally_authorized_aliases_retain_both_native_boundaries() {
        let fixture = Fixture::new();
        let target = fixture.work.join("target");
        fs::create_dir(&target).unwrap();
        let alias = fixture.work.join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let policy = fixture.policy(&[SandboxRule {
            path: alias.to_string_lossy().into_owned(),
            action: SandboxRuleAction::AllowWrite,
        }]);
        let rules = process_rules(&policy).unwrap();
        assert!(rules.iter().any(|rule| rule.path == alias && rule.write));
        assert!(rules.iter().any(|rule| rule.path == target && rule.write));
    }

    #[tokio::test]
    async fn owner_cancellation_settles_the_actual_native_execution() {
        let fixture = Fixture::new();
        let launcher = fixture.launcher(false);
        let lifetime = Arc::new(ProcessLifetime::default());
        let execution = launcher
            .launch(
                command("while :; do :; done"),
                SharedSecurity(fixture.policy(&[])),
                lifetime.clone(),
            )
            .await
            .unwrap();
        lifetime.cancel();
        tokio::time::timeout(Duration::from_secs(10), execution.settle())
            .await
            .unwrap()
            .unwrap();
        assert!(execution.is_completed());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unavailable_sandbox_never_retries_the_command_unconfined() {
        use parrot_tools::{
            process::execution::ProcessRunner, sandbox::linux::LinuxBubblewrapSandbox,
        };
        let fixture = Fixture::new();
        let marker = fixture.work.join("must-not-exist");
        let launcher = ProductionLauncher {
            runner: ProcessRunner::new(
                LinuxBubblewrapSandbox::new(Path::new(""), false).unwrap(),
                Arc::new(SandboxGate::new(true)),
            ),
            launch_directory: fixture.work.clone(),
            queue_directory: fixture.session.join("queues"),
        };
        let request = command(&format!("printf bypass > '{}'", marker.display()));
        let error = launcher
            .launch(
                request,
                SharedSecurity(fixture.policy(&[])),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!marker.exists());
    }
}
