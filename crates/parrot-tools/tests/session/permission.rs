use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        paths::ResolvedRead,
        security::{AccessSnapshot, InvocationContext},
        tools::Tool,
    },
    values::tools::ConfiguredToolDefinition,
};
use parrot_tools::session::{
    Failure, FailureKind, Invocation, PermissionReply, PermissionServices,
    RequestWritePermissionTool, WriteTarget, WriteTargetKind,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

struct Snapshot {
    allowed: bool,
    read_only: bool,
}
impl AccessSnapshot for Snapshot {
    type Error = Failure;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Failure> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), Failure> {
        if self.allowed {
            Ok(())
        } else {
            Err(Failure::new(FailureKind::Permission, "denied"))
        }
    }
}
struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if !self.0 {
                std::future::pending::<()>().await;
            }
        })
    }
}
struct Services {
    requests: Mutex<Vec<(String, Vec<WriteTarget>)>>,
    reply: PermissionReply,
}
impl PermissionServices for Services {
    type Security = Snapshot;
    type Selection = ();
    fn resolve_target(&self, requested: &str) -> Result<WriteTarget, Failure> {
        if requested == "/missing" {
            return Err(Failure::new(
                FailureKind::Io,
                "The security write target does not exist.",
            ));
        }
        if requested.is_empty() || !Path::new(requested).is_absolute() {
            return Err(Failure::new(
                FailureKind::Argument,
                "The security write target path must be absolute.",
            ));
        }
        Ok(WriteTarget {
            path: PathBuf::from(requested),
            kind: WriteTargetKind::File,
        })
    }
    fn allows_write(&self, snapshot: &Snapshot, _: &WriteTarget) -> bool {
        snapshot.allowed
    }
    fn is_read_only(&self, snapshot: &Snapshot) -> bool {
        snapshot.read_only
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
            self.requests
                .lock()
                .unwrap()
                .push((reason.to_owned(), targets));
            Ok(self.reply.clone())
        })
    }
}
async fn run(
    arguments: &str,
    snapshot: Snapshot,
    reply: PermissionReply,
    token: bool,
) -> (Result<String, OperationError<Failure>>, Arc<Services>) {
    let services = Arc::new(Services {
        requests: Mutex::new(Vec::new()),
        reply,
    });
    let tool = RequestWritePermissionTool::new(
        ConfiguredToolDefinition::new("configured", "{}"),
        services.clone(),
    );
    let invocation = Invocation {
        call_id: "permission".into(),
        assistant_sequence: 4,
        arguments_json: arguments.into(),
        prompt_templates: None,
    };
    let token = Token(token);
    let result = tool
        .invoke(invocation, InvocationContext::new(snapshot, (), &token))
        .await
        .map(|output| output.text);
    (result, services)
}

#[tokio::test]
async fn trims_reason_and_deduplicates_resolved_targets_without_reordering() {
    let (result, services) = run(
        r#"{"paths":["/b","/a","/b"],"reason":"  update  "}"#,
        Snapshot {
            allowed: false,
            read_only: false,
        },
        PermissionReply::Granted,
        false,
    )
    .await;
    assert_eq!(
        result.unwrap(),
        "The agent session security profile now allows writing: /b, /a"
    );
    let requests = services.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "update");
    assert_eq!(
        requests[0]
            .1
            .iter()
            .map(|target| target.path.as_path())
            .collect::<Vec<_>>(),
        vec![Path::new("/b"), Path::new("/a")]
    );
}

#[tokio::test]
async fn already_allowed_precedes_read_only_and_does_not_register() {
    let (result, services) = run(
        r#"{"paths":["/file"],"reason":"edit"}"#,
        Snapshot {
            allowed: true,
            read_only: true,
        },
        PermissionReply::Granted,
        false,
    )
    .await;
    assert_eq!(
        result.unwrap(),
        "Write permission already allowed by the current security profile: /file"
    );
    assert!(services.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn denied_read_only_returns_error_before_broker_request() {
    let (result, services) = run(
        r#"{"paths":["/file"],"reason":"edit"}"#,
        Snapshot {
            allowed: false,
            read_only: true,
        },
        PermissionReply::Granted,
        false,
    )
    .await;
    assert_eq!(
        result.unwrap(),
        "error: request_write_permission is not permitted by the current security profile"
    );
    assert!(services.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_arguments_and_unresolved_targets_do_not_request_or_grant() {
    for arguments in [
        "null",
        "[]",
        "{",
        "{}",
        r#"{"paths":[],"reason":"edit"}"#,
        r#"{"paths":["/file"],"reason":" "}"#,
        r#"{"paths":["/file"]}"#,
        r#"{"paths":["/missing"],"reason":"edit"}"#,
        r#"{"paths":["relative"],"reason":"edit"}"#,
        r#"{"paths":[null],"reason":"edit"}"#,
        r#"{"paths":["/file"],"reason":"edit","extra":true}"#,
    ] {
        let (result, services) = run(
            arguments,
            Snapshot {
                allowed: false,
                read_only: false,
            },
            PermissionReply::Granted,
            false,
        )
        .await;
        assert!(result.unwrap().starts_with("error: "), "{arguments}");
        assert!(services.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn preserves_rejection_reason_and_normal_user_away_result() {
    for (reply, expected) in [
        (
            PermissionReply::Rejected {
                reason: String::new(),
            },
            "Write permission request rejected.",
        ),
        (
            PermissionReply::Rejected {
                reason: "  no  ".into(),
            },
            "Write permission request rejected:   no  ",
        ),
        (PermissionReply::UserAway, "The user is away."),
    ] {
        let (result, services) = run(
            r#"{"paths":["/file"],"reason":"edit"}"#,
            Snapshot {
                allowed: false,
                read_only: false,
            },
            reply,
            false,
        )
        .await;
        assert_eq!(result.unwrap(), expected);
        assert_eq!(services.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancellation_is_not_an_approval_or_error_text() {
    let (result, services) = run(
        r#"{"paths":["/file"],"reason":"edit"}"#,
        Snapshot {
            allowed: false,
            read_only: false,
        },
        PermissionReply::Granted,
        true,
    )
    .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert!(services.requests.lock().unwrap().is_empty());
}
