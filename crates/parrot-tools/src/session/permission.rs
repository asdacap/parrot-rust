use super::{
    Failure, FailureKind, Invocation, Output, input,
    tools::{error_output, text_output},
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        security::{AccessSnapshot, InvocationContext},
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteTargetKind {
    File,
    Directory,
}

/// A resolver report, not an authorization or open-file capability. The actual
/// approval adapter must validate the target again before changing policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteTarget {
    pub path: PathBuf,
    pub kind: WriteTargetKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionReply {
    UserAway,
    Granted,
    Rejected { reason: String },
}

/// Bound to the requesting agent's identity/security and actual broker. Target
/// resolution and approval are supplied by the owning security adapter, not
/// implemented by this wrapper or inferred from a returned path report.
pub trait PermissionServices: Send + Sync {
    type Security: AccessSnapshot + 'static;
    type Selection: Send + Sync + 'static;

    fn resolve_target(&self, requested: &str) -> Result<WriteTarget, Failure>;
    fn allows_write(&self, snapshot: &Self::Security, target: &WriteTarget) -> bool;
    fn is_read_only(&self, snapshot: &Self::Security) -> bool;
    fn request<'a>(
        &'a self,
        reason: &'a str,
        targets: Vec<WriteTarget>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<PermissionReply, Failure>>;
}

pub struct RequestWritePermissionTool<P: PermissionServices> {
    definition: ConfiguredToolDefinition,
    services: Arc<P>,
}

impl<P: PermissionServices> RequestWritePermissionTool<P> {
    pub fn new(definition: ConfiguredToolDefinition, services: Arc<P>) -> Self {
        Self {
            definition,
            services,
        }
    }

    pub fn name(&self) -> &'static str {
        "request_write_permission"
    }

    async fn execute(
        &self,
        invocation: &Invocation,
        snapshot: &P::Security,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Output, Failure> {
        let object = input::object(&invocation.arguments_json, &["paths", "reason"])
            .map_err(OperationError::Failed)?;
        let paths = match object.get("paths") {
            None | Some(Value::Null) => {
                return Err(OperationError::Failed(input::format(
                    "Tool arguments require an array 'paths'.",
                )));
            }
            Some(Value::Array(paths)) => paths,
            Some(_) => {
                return Err(OperationError::Failed(Failure::new(
                    FailureKind::Json,
                    "Tool argument 'paths' must be an array.",
                )));
            }
        };
        let reason = input::required_string(&object, "reason")
            .map_err(OperationError::Failed)?
            .trim();
        if paths.is_empty() {
            return Err(OperationError::Failed(input::format(
                "Tool arguments require at least one path.",
            )));
        }
        if reason.is_empty() {
            return Err(OperationError::Failed(input::format(
                "Permission reason must not be empty.",
            )));
        }
        let mut targets = Vec::new();
        for path in paths {
            let path = path.as_str().ok_or_else(|| {
                OperationError::Failed(Failure::new(
                    FailureKind::Json,
                    "Permission paths must be strings.",
                ))
            })?;
            let target = self
                .services
                .resolve_target(path)
                .map_err(OperationError::Failed)?;
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        let display = targets
            .iter()
            .map(|target| target.path.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        if targets
            .iter()
            .all(|target| self.services.allows_write(snapshot, target))
        {
            return Ok(format!(
                "Write permission already allowed by the current security profile: {display}"
            )
            .into());
        }
        if self.services.is_read_only(snapshot) {
            return Err(OperationError::Failed(Failure::new(
                FailureKind::Permission,
                "request_write_permission is not permitted by the current security profile",
            )));
        }
        match self.services.request(reason, targets, cancellation).await? {
            PermissionReply::UserAway => text_output(invocation, "The user is away."),
            PermissionReply::Granted => Ok(format!(
                "The agent session security profile now allows writing: {display}"
            )
            .into()),
            PermissionReply::Rejected { reason } if reason.is_empty() => {
                Ok("Write permission request rejected.".to_owned().into())
            }
            PermissionReply::Rejected { reason } => {
                Ok(format!("Write permission request rejected: {reason}").into())
            }
        }
    }
}

impl<P: PermissionServices> Tool for RequestWritePermissionTool<P> {
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Security = P::Security;
    type Selection = P::Selection;
    type Output = Output;
    type Error = Failure;

    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<Output, Failure>> {
        Box::pin(async move {
            match self
                .execute(&invocation, context.security(), context.cancellation())
                .await
            {
                Err(OperationError::Failed(error))
                    if matches!(
                        error.kind,
                        FailureKind::Json
                            | FailureKind::Format
                            | FailureKind::Argument
                            | FailureKind::Io
                            | FailureKind::Unauthorized
                            | FailureKind::Permission
                    ) =>
                {
                    error_output(&invocation, &error.message)
                }
                result => result,
            }
        })
    }
}
