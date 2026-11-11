//! Native process execution bound to one user workspace and an immutable turn capture.

use super::policy::{ProductionAccessRule, ProductionPolicy};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation},
    paths::ResolvedRead,
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
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The user-session constructor supplies these resources once. In particular,
/// command environment overrides cannot select another agent's scratch or queue.
pub struct ProductionLauncher {
    runner: ConfiguredProcessRunner,
    launch_directory: PathBuf,
    session_root: PathBuf,
}

impl ProductionLauncher {
    pub fn with_device_paths(
        gate: Arc<SandboxGate>,
        launch_directory: PathBuf,
        session_root: PathBuf,
        device_paths: Vec<PathBuf>,
    ) -> io::Result<Self> {
        if !launch_directory.is_absolute() || !session_root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process resource paths must be absolute",
            ));
        }
        let runner = locate_configured(&ExecutableLocator::capture(), gate, device_paths)?;
        Ok(Self {
            runner,
            launch_directory,
            session_root,
        })
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
                || !scratch.root().starts_with(&self.session_root)
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
                ))
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            let resources = ProcessResources::new(self.launch_directory.clone(), scratch.clone());
            let rules = if self.runner.sandbox_gate().enabled() {
                Some(process_rules(policy)?)
            } else {
                None
            };
            let mut start = ProcessStartRequest::new(request.command, resources);
            start.environment =
                with_tmp_environment_defaults(rules.as_deref(), request.environment);
            start.terminal_mode = request.terminal_mode;
            start.cancellation = Some(lifetime);

            // ProcessRunner alone selects the sandbox before materializing its
            // policy. A failed sandbox start is never retried outside the sandbox.
            let execution = self
                .runner
                .start_with_policy(&start, move || {
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    let rules = rules
                        .expect("sandboxed starts materialize rules before the policy factory");
                    #[cfg(target_os = "linux")]
                    let sandbox_policy = parrot_tools::sandbox::linux::LinuxSandboxPolicy {
                        rules: rules
                            .into_iter()
                            .map(|rule| parrot_tools::sandbox::linux::LinuxAccessRule {
                                path: rule.path,
                                read: rule.read,
                                write: rule.write,
                            })
                            .collect(),
                        owned_roots: vec![scratch.root().to_path_buf()],
                        device_paths: Vec::new(),
                    };
                    #[cfg(target_os = "macos")]
                    let sandbox_policy =
                        parrot_tools::sandbox::macos::SeatbeltPolicy::compile_rules(
                            rules.into_iter().map(|rule| {
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
                    Ok(sandbox_policy)
                })
                .await?;
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

/// Shell tool children address tmp through the sandbox-mapped /tmp instead of
/// an inherited parent TMPDIR, whose length is capped by sockaddr_un. The host
/// path stays reachable through PARROT_HOST_TMPDIR. Rules are absent when the
/// sandbox is off, and then /tmp is the unmapped host directory. Overrides win
/// over defaults.
fn with_tmp_environment_defaults(
    rules: Option<&[ProductionAccessRule]>,
    overrides: Vec<(String, String)>,
) -> Vec<(String, String)> {
    if !cfg!(target_os = "linux") {
        return overrides;
    }
    let mut defaults = vec![(
        "PARROT_HOST_TMPDIR".to_owned(),
        std::env::temp_dir().to_string_lossy().into_owned(),
    )];
    if rules.is_none_or(|rules| {
        rules
            .iter()
            .any(|rule| rule.path.as_path() == Path::new("/tmp") && rule.write)
    }) {
        defaults.push(("TMPDIR".to_owned(), "/tmp".to_owned()));
    }
    let mut environment: Vec<(String, String)> = defaults
        .into_iter()
        .filter(|(key, _)| {
            !overrides
                .iter()
                .any(|(override_key, _)| override_key == key)
        })
        .collect();
    environment.extend(overrides);
    environment
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
            ProductionLauncher::with_device_paths(
                Arc::new(SandboxGate::new(enabled)),
                self.work.clone(),
                self.session.clone(),
                Vec::new(),
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
        let mut foreign = Fixture::new();
        foreign.work = fixture.work.clone();
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
            ("AGENT_DIR".into(), "/not-an-authorized-resource".into()),
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
        assert!(fixture.scratch.queue_directory().is_dir());
        assert!(!fixture.session.join("queues").exists());
        assert!(execution.is_completed());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_children_receive_tmp_environment_defaults() {
        let fixture = Fixture::new();
        let execution = fixture
            .launcher(false)
            .launch(
                command("printf '%s|%s' \"${TMPDIR-}\" \"${PARROT_HOST_TMPDIR-}\""),
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
            format!("/tmp|{}", std::env::temp_dir().display())
        );
        assert!(execution.is_completed());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn explicit_tmpdir_override_wins_over_the_default() {
        let fixture = Fixture::new();
        let mut request = command("printf '%s|%s' \"${TMPDIR-}\" \"${PARROT_HOST_TMPDIR-}\"");
        request.environment = vec![("TMPDIR".into(), "/custom-tmp".into())];
        let execution = fixture
            .launcher(false)
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
            format!("/custom-tmp|{}", std::env::temp_dir().display())
        );
        assert!(execution.is_completed());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_children_receive_tmp_default_from_a_writable_tmp_rule() {
        let fixture = Fixture::new();
        let policy = fixture.policy(&[SandboxRule {
            path: "/tmp".to_owned(),
            action: SandboxRuleAction::AllowWrite,
        }]);
        let execution = fixture
            .launcher(true)
            .launch(
                command("printf '%s|%s' \"${TMPDIR-}\" \"${PARROT_HOST_TMPDIR-}\""),
                SharedSecurity(policy),
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
            format!("/tmp|{}", std::env::temp_dir().display())
        );
        assert!(execution.is_completed());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_children_without_a_tmp_rule_inherit_the_source_tmpdir() {
        let fixture = Fixture::new();
        let expected = std::env::var("TMPDIR").unwrap_or_default();
        let execution = fixture
            .launcher(true)
            .launch(
                command("printf '%s|%s' \"${TMPDIR-}\" \"${PARROT_HOST_TMPDIR-}\""),
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
            format!("{}|{}", expected, std::env::temp_dir().display())
        );
        assert!(execution.is_completed());
    }

    #[tokio::test]
    async fn child_execution_provisions_queues_in_the_child_agent_directory() {
        let fixture = Fixture::new();
        let launcher = fixture.launcher(false);
        let scratch =
            AgentScratchDirectory::new(&fixture.scratch.root().join("child"), &fixture.root)
                .unwrap();
        let materializer = super::super::policy::ProductionPolicyMaterializer::new();
        materializer
            .register_session(
                fixture.session.clone(),
                ProjectWorkspace::from_resolved(
                    fixture.work.clone(),
                    fixture.work.clone(),
                    None,
                    None,
                ),
            )
            .unwrap();
        let policy = materializer
            .for_agent(
                &Arc::new(ProductionPolicy::compose(false, &[], &[], &[]).unwrap()),
                &scratch,
            )
            .unwrap();
        let execution = launcher
            .launch(
                command("exit 0"),
                SharedSecurity(policy),
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
        assert!(scratch.queue_directory().is_dir());
        assert!(!fixture.scratch.queue_directory().exists());
        assert!(!fixture.session.join("queues").exists());
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn explicit_gate_off_skips_native_policy_materialization() {
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

        let marker = fixture.work.join("command-marker");
        let text = format!("printf owned > '{}'; printf unconfined", marker.display());
        let error = fixture
            .launcher(true)
            .launch(
                command(&text),
                SharedSecurity(policy.clone()),
                Arc::new(ProcessLifetime::default()),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!marker.exists());

        let execution = fixture
            .launcher(false)
            .launch(
                command(&text),
                SharedSecurity(policy),
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
        assert_eq!(result.stdout, "unconfined");
        assert_eq!(fs::read_to_string(marker).unwrap(), "owned");
        assert!(execution.is_completed());
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
            session_root: fixture.session.clone(),
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
