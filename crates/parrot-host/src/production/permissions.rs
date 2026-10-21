//! Scoped durable permission requests over real, identity-bound write targets.

use super::policy::{ProductionPolicy, ProductionPolicyMaterializer, ProductionWriteTarget};
use crate::composition::{
    context::ContextSelection,
    factory::user::UserServices,
    protocol::{self, RpcCode, RpcError},
};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_foundation::paths::AgentScratchDirectory;
use parrot_runtime::{
    children::{AgentIdentity, Scope},
    interactions::{
        InteractionError, InteractionErrorKind, PermissionBroker, PermissionDecision,
        PermissionReplyKind, PermissionRequest, TargetApproval,
    },
    tool_settlement::SharedSecurity,
};
use parrot_tools::session::{
    Failure, FailureKind, PermissionReply, PermissionServices, WriteTarget,
};
use std::{
    path::PathBuf,
    sync::{Arc, Weak},
};

pub struct ProductionPermissionAuthority {
    user: Weak<UserServices<ProductionPolicy>>,
    identity: Arc<AgentIdentity>,
    broker: Arc<PermissionBroker<WriteTarget>>,
    materializer: Arc<ProductionPolicyMaterializer>,
    scratch: AgentScratchDirectory,
}

impl ProductionPermissionAuthority {
    pub fn new(
        user: Arc<UserServices<ProductionPolicy>>,
        identity: Arc<AgentIdentity>,
        materializer: Arc<ProductionPolicyMaterializer>,
    ) -> protocol::Result<Self> {
        let names: Vec<_> = identity.name_path.iter().map(String::as_str).collect();
        let scratch = user
            .resources
            .agent_scratch(&names)
            .map_err(|error| RpcError::new(RpcCode::Internal, error.to_string()))?;
        Ok(Self {
            user: Arc::downgrade(&user),
            identity,
            broker: user.permissions.clone(),
            materializer,
            scratch,
        })
    }

    fn resolve(&self, requested: &str) -> Result<ProductionWriteTarget, Failure> {
        ProductionWriteTarget::resolve(&PathBuf::from(requested)).map_err(permission_failure)
    }
}

impl PermissionServices for ProductionPermissionAuthority {
    type Security = SharedSecurity<ProductionPolicy>;
    type Selection = ContextSelection<ProductionPolicy>;

    fn resolve_target(&self, requested: &str) -> Result<WriteTarget, Failure> {
        let target = self.resolve(requested)?;
        Ok(describe(&target))
    }

    fn allows_write(&self, snapshot: &Self::Security, target: &WriteTarget) -> bool {
        snapshot.snapshot().allows_write(&target.path)
    }

    fn is_read_only(&self, snapshot: &Self::Security) -> bool {
        snapshot.snapshot().is_read_only()
    }

    fn request<'a>(
        &'a self,
        reason: &'a str,
        targets: Vec<WriteTarget>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<PermissionReply, Failure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if self.identity.session.depth != 0 {
                return Err(OperationError::Failed(Failure::new(
                    FailureKind::Permission,
                    "only the root agent may request additional write permission",
                )));
            }
            let bound_targets: Vec<_> = targets
                .iter()
                .map(|target| {
                    let bound =
                        ProductionWriteTarget::resolve(&target.path).map_err(permission_failure)?;
                    if describe(&bound) != *target {
                        return Err(Failure::new(
                            FailureKind::Permission,
                            "the requested write target changed before publication",
                        ));
                    }
                    Ok(bound)
                })
                .collect::<Result<_, _>>()
                .map_err(OperationError::Failed)?;
            let approval = Arc::new(ProductionApproval {
                user: self.user.clone(),
                identity: self.identity.clone(),
                materializer: self.materializer.clone(),
                scratch: self.scratch.clone(),
                targets: bound_targets,
            });
            let reply = self
                .broker
                .request(
                    PermissionRequest {
                        agent_session_id: self.identity.session.session_id.clone(),
                        reason: reason.to_owned(),
                        targets,
                        approval,
                    },
                    cancellation,
                )
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => {
                        OperationError::Failed(permission_failure(error))
                    }
                })?;
            Ok(if reply.kind == PermissionReplyKind::UserAway {
                PermissionReply::UserAway
            } else if reply.decision == PermissionDecision::Grant {
                PermissionReply::Granted
            } else {
                PermissionReply::Rejected {
                    reason: reply.reason,
                }
            })
        })
    }
}

struct ProductionApproval {
    user: Weak<UserServices<ProductionPolicy>>,
    identity: Arc<AgentIdentity>,
    materializer: Arc<ProductionPolicyMaterializer>,
    scratch: AgentScratchDirectory,
    targets: Vec<ProductionWriteTarget>,
}

impl ProductionApproval {
    fn scope(&self) -> Result<Arc<Scope<ProductionPolicy>>, InteractionError> {
        let user = self
            .user
            .upgrade()
            .ok_or_else(|| interaction_error("the permission's user session has closed"))?;
        let registry = user.registry().map_err(interaction_error)?;
        registry
            .snapshot_scopes()
            .into_iter()
            .find(|scope| Arc::ptr_eq(scope.identity(), &self.identity))
            .ok_or_else(|| interaction_error("the requesting agent scope is no longer active"))
    }
}

impl TargetApproval<WriteTarget> for ProductionApproval {
    fn validate(&self, targets: &[WriteTarget]) -> Result<(), InteractionError> {
        if self.identity.session.depth != 0
            || targets.len() != self.targets.len()
            || targets
                .iter()
                .zip(&self.targets)
                .any(|(given, bound)| *given != describe(bound))
        {
            return Err(interaction_error(
                "write permission targets do not match the requesting scope",
            ));
        }
        for target in &self.targets {
            target.validate().map_err(interaction_error)?;
        }
        let selected = self
            .scope()?
            .selection()
            .current()
            .map_err(interaction_error)?;
        if selected.security.is_read_only() {
            return Err(interaction_error(
                "write permission cannot be granted in a read-only profile",
            ));
        }
        Ok(())
    }

    fn approve(&self, targets: &[WriteTarget]) -> Result<(), InteractionError> {
        // Revalidate the exact physical target identities at reply time, not
        // merely when displaying a path in the permission prompt.
        self.validate(targets)?;
        let scope = self.scope()?;
        // Lock order is selected state -> materializer state. Other selection
        // construction releases materializer state before updating the cell;
        // the materializer never calls back into a selected-state lock.
        scope.selection().try_update_security(|current| {
            self.materializer
                .approve_for_agent(current, &self.scratch, &self.targets)
                .map_err(interaction_error)
        })
    }
}

fn describe(target: &ProductionWriteTarget) -> WriteTarget {
    WriteTarget {
        path: target.path().to_path_buf(),
        kind: target.kind(),
    }
}

fn permission_failure(error: impl std::fmt::Display) -> Failure {
    Failure::new(FailureKind::Permission, error.to_string())
}

fn interaction_error(error: impl std::fmt::Display) -> InteractionError {
    InteractionError::new(InteractionErrorKind::Permission, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_foundation::template_catalog::PromptTemplateCatalog;
    use parrot_tools::session::WriteTargetKind;
    use std::{
        collections::BTreeMap,
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        scratch: AgentScratchDirectory,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "parrot-production-permission-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            let scratch =
                AgentScratchDirectory::new(&root.join("session/root-agents/main"), &root).unwrap();
            Self { root, scratch }
        }
        fn approval(&self, target: ProductionWriteTarget) -> ProductionApproval {
            let environment: BTreeMap<_, _> = [
                ("HOME", "home"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_DATA_HOME", "data"),
                ("WORKDIR", "work"),
            ]
            .into_iter()
            .map(|(key, directory)| {
                (
                    key.to_owned(),
                    self.root.join(directory).to_string_lossy().into_owned(),
                )
            })
            .collect();
            let templates = Arc::new(
                PromptTemplateCatalog::from_content("", &environment)
                    .unwrap()
                    .1,
            );
            ProductionApproval {
                user: Weak::new(),
                identity: AgentIdentity::root("agent-identity".into(), "main".into(), templates),
                materializer: Arc::new(ProductionPolicyMaterializer::new()),
                scratch: self.scratch.clone(),
                targets: vec![target],
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn replacing_an_approved_inode_is_rejected_before_any_policy_update() {
        let fixture = Fixture::new();
        let path = fixture.root.join("target");
        fs::write(&path, "first").unwrap();
        let bound = ProductionWriteTarget::resolve(&path).unwrap();
        let requested = describe(&bound);
        let approval = fixture.approval(bound);
        fs::rename(&path, fixture.root.join("retained-original")).unwrap();
        fs::write(&path, "replacement").unwrap();
        let failure = approval.approve(&[requested]).unwrap_err();
        assert!(failure.message.contains("changed after approval"));
    }

    #[test]
    fn changing_target_kind_or_path_cannot_substitute_a_broader_grant() {
        let fixture = Fixture::new();
        let path = fixture.root.join("file");
        fs::write(&path, "bytes").unwrap();
        let bound = ProductionWriteTarget::resolve(&path).unwrap();
        let approval = fixture.approval(bound);
        let broadened = WriteTarget {
            path: fixture.root.clone(),
            kind: WriteTargetKind::Directory,
        };
        let failure = approval.approve(&[broadened]).unwrap_err();
        assert!(failure.message.contains("do not match"));
        let relabeled = WriteTarget {
            path,
            kind: WriteTargetKind::Directory,
        };
        assert!(
            approval
                .validate(&[relabeled])
                .unwrap_err()
                .message
                .contains("do not match")
        );
    }

    #[test]
    fn valid_targets_cannot_grant_after_the_actual_user_scope_is_gone() {
        let fixture = Fixture::new();
        let path = fixture.root.join("file");
        fs::write(&path, "bytes").unwrap();
        let bound = ProductionWriteTarget::resolve(&path).unwrap();
        let requested = describe(&bound);
        let failure = fixture.approval(bound).approve(&[requested]).unwrap_err();
        assert!(failure.message.contains("user session has closed"));
    }
}
