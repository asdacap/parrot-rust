#![allow(dead_code)]

// Reuse the accepted real scope, registry, provider and selected-root storage fixture.
#[path = "../../../parrot-runtime/tests/children/admission_publisher.rs"]
mod admission_publisher;
#[path = "../../../parrot-runtime/tests/agent_queues/harness.rs"]
mod harness;
#[path = "../../../parrot-runtime/tests/children/support.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
            LinkPolicy, ReadRequest,
        },
        runtime::AgentQueues as QueuePort,
        security::InvocationContext,
        tools::Tool,
    },
    values::tools::{ConfiguredToolDefinition, ToolDefinitionCatalog},
};
use parrot_runtime::{
    agent_queues::{AgentQueues, QueueError},
    children::{AgentRegistry, AgentResolver, ChildError, Scope},
};
use parrot_storage::{events::NeverCancelled, queue_files as storage};
use parrot_tools::tasks_queues::{
    FailureKind, ToolFailure, ToolInvocation, ToolOutput,
    queues::{
        CreateRequest, Direction, InfoRequest, PushRequest, QueueDiagnostic, QueueDiagnostics,
        QueueInfo, QueueTarget, QueueTool, QueueToolBackend, QueueToolKind, ResolvedQueue,
        TakeRequest, TakeResult,
    },
};
use tokio::sync::Semaphore;

pub use support::Security;

/// A resolved address retains precisely the service and scope selected by AgentResolver.
#[derive(Clone)]
pub struct Target {
    _scope: Arc<Scope<Security>>,
    queues: Arc<AgentQueues<Security>>,
    session_id: String,
}

pub struct Backend {
    scope: Arc<Scope<Security>>,
    _registry: Arc<AgentRegistry<Security>>,
    queues: Arc<AgentQueues<Security>>,
    resolver: AgentResolver<Security>,
}
impl Backend {
    pub fn new(scope: Arc<Scope<Security>>, registry: Arc<AgentRegistry<Security>>) -> Self {
        Self {
            queues: scope.get_service().unwrap(),
            resolver: AgentResolver::new(&scope, &registry),
            scope,
            _registry: registry,
        }
    }
}

fn info(value: storage::QueueInfo) -> QueueInfo {
    QueueInfo {
        path: value.path.to_str().unwrap().to_owned(),
        name: value.name,
        description: value.description,
        size: i32::try_from(value.size).unwrap(),
        closed: value.closed,
    }
}
fn direction(value: Direction) -> storage::Direction {
    match value {
        Direction::Unspecified => storage::Direction::Unspecified,
        Direction::Front => storage::Direction::Front,
        Direction::Back => storage::Direction::Back,
    }
}
fn scope_failure(error: ChildError) -> OperationError<ToolFailure> {
    if error.code() == "cancelled" {
        return OperationError::Cancelled;
    }
    let kind = match &error {
        ChildError::Registry(_) | ChildError::NameConflict(_) => FailureKind::Registry,
        _ => match error.code() {
            "argument" => FailureKind::Argument,
            "invalid_operation" => FailureKind::InvalidOperation,
            "access_denied" => FailureKind::Denied,
            "io" => FailureKind::Io,
            _ => FailureKind::Other,
        },
    };
    OperationError::Failed(ToolFailure::new(kind, error.to_string()))
}
fn queue_failure(error: QueueError) -> OperationError<ToolFailure> {
    match error {
        QueueError::Scope(error) => scope_failure(error),
        QueueError::Storage(error) => {
            let kind = match error.kind {
                storage::ErrorKind::Cancelled | storage::ErrorKind::CancelledBeforeWait => {
                    return OperationError::Cancelled;
                }
                storage::ErrorKind::Argument => FailureKind::Argument,
                storage::ErrorKind::Inventory | storage::ErrorKind::Disposed => {
                    FailureKind::InvalidOperation
                }
                storage::ErrorKind::Io => FailureKind::Io,
                storage::ErrorKind::UnauthorizedAccess => FailureKind::Denied,
                storage::ErrorKind::Queue
                | storage::ErrorKind::NotFound
                | storage::ErrorKind::AlreadyExists
                | storage::ErrorKind::Closed
                | storage::ErrorKind::Empty => FailureKind::Queue,
            };
            OperationError::Failed(ToolFailure::new(kind, error.to_string()))
        }
    }
}

impl QueuePort for Backend {
    type CreateRequest = CreateRequest;
    type CreateResult = QueueInfo;
    type InfoRequest = InfoRequest<Target>;
    type InfoResult = QueueInfo;
    type PushRequest = PushRequest<Target>;
    type PushResult = QueueInfo;
    type TakeRequest = TakeRequest<Target>;
    type TakeResult = TakeResult;
    type Error = ToolFailure;

    fn create<'a>(
        &'a self,
        request: Self::CreateRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            self.queues
                .create(&request.name, &request.description)
                .map(info)
                .map_err(queue_failure)
        })
    }
    fn info<'a>(
        &'a self,
        request: Self::InfoRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            match request.target {
                QueueTarget::Invoking(name) => self.queues.get(&name),
                QueueTarget::Local(target) => target
                    .owner
                    .queues
                    .local()
                    .get(&target.name)
                    .map_err(Into::into),
            }
            .map(info)
            .map_err(queue_failure)
        })
    }
    fn push<'a>(
        &'a self,
        request: Self::PushRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            let result = match request.target {
                QueueTarget::Invoking(name) => {
                    cancellation
                        .check()
                        .map_err(|_| OperationError::Cancelled)?;
                    if let Some(items) = request.items.iter().cloned().collect::<Option<Vec<_>>>() {
                        self.queues.push(
                            &name,
                            &items,
                            direction(request.direction),
                            request.close,
                            cancellation,
                        )
                    } else {
                        let (store, name) = self
                            .queues
                            .resolve_address(&self.resolver, &name)
                            .map_err(queue_failure)?;
                        store
                            .push_nullable(
                                &name,
                                &request.items,
                                direction(request.direction),
                                request.close,
                            )
                            .map_err(Into::into)
                    }
                }
                QueueTarget::Local(target) => {
                    cancellation
                        .check()
                        .map_err(|_| OperationError::Cancelled)?;
                    target
                        .owner
                        .queues
                        .local()
                        .push_nullable(
                            &target.name,
                            &request.items,
                            direction(request.direction),
                            request.close,
                        )
                        .map_err(Into::into)
                }
            };
            result.map(info).map_err(queue_failure)
        })
    }
    fn take<'a>(
        &'a self,
        request: Self::TakeRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<TakeResult, ToolFailure>> {
        Box::pin(async move {
            let result = match request.target {
                QueueTarget::Invoking(name) => {
                    self.queues
                        .try_take(&name, request.count, direction(request.direction))
                }
                QueueTarget::Local(target) => target
                    .owner
                    .queues
                    .local()
                    .try_take(&target.name, request.count, direction(request.direction))
                    .map_err(Into::into),
            };
            match result {
                Ok(taken) => Ok(TakeResult {
                    acquired: taken.acquired,
                    info: taken.info.map(info),
                    items: taken.items,
                }),
                // The actual store reports open-empty as an error carrying the current info.
                // Translate only that case into the Tool's retryable poll result.
                Err(QueueError::Storage(error)) if error.kind == storage::ErrorKind::Empty => {
                    Ok(TakeResult {
                        acquired: false,
                        info: error.info.map(info),
                        items: Vec::new(),
                    })
                }
                Err(error) => Err(queue_failure(error)),
            }
        })
    }
}
impl QueueToolBackend<Target> for Backend {
    fn resolve(&self, address: &str) -> Result<ResolvedQueue<Target>, ToolFailure> {
        let (scope, name) = self.resolver.resolve_resource(address).map_err(|error| {
            match scope_failure(error) {
                OperationError::Failed(error) => error,
                OperationError::Cancelled => unreachable!("resource resolution is synchronous"),
            }
        })?;
        let queues = scope
            .get_service()
            .map_err(|error| match scope_failure(error) {
                OperationError::Failed(error) => error,
                OperationError::Cancelled => unreachable!("service lookup is synchronous"),
            })?;
        Ok(ResolvedQueue {
            owner: Target {
                session_id: scope.identity().session.session_id.clone(),
                queues,
                _scope: scope,
            },
            name,
        })
    }
    fn session_id(&self) -> &str {
        &self.scope.identity().session.session_id
    }
    fn resolved_session_id<'a>(&self, target: &'a Target) -> &'a str {
        &target.session_id
    }
}

/// Inline push tests must never acquire any filesystem reading authority.
pub struct UnavailableReader;
fn unavailable<T>() -> OperationResult<T, FileReadError> {
    Err(OperationError::Failed(FileReadError::new(
        FileReadErrorKind::Denied,
        "runtime queue fixture does not authorize file reads",
    )))
}
impl AuthorizedReadService for UnavailableReader {
    type Security = Security;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async { unavailable() })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async { unavailable() })
    }
    fn read<'a>(
        &'a self,
        _: &'a ReadRequest,
        _: &'a Security,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async { unavailable() })
    }
}

pub struct RealClock {
    pub sleeps: Mutex<Vec<Duration>>,
    sleeping: Semaphore,
}
impl Default for RealClock {
    fn default() -> Self {
        Self {
            sleeps: Mutex::new(Vec::new()),
            sleeping: Semaphore::new(0),
        }
    }
}
impl RealClock {
    pub async fn sleeping(&self) {
        tokio::time::timeout(Duration::from_secs(1), self.sleeping.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
}
impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            self.sleeps
                .lock()
                .unwrap()
                .push(deadline.saturating_duration_since(Instant::now()));
            self.sleeping.add_permits(1);
            tokio::select! {
                _ = cancellation.cancelled() => Err(Cancelled),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => cancellation.check(),
            }
        })
    }
}
#[derive(Default)]
pub struct DiagnosticLog(pub Mutex<Vec<QueueDiagnostic>>);
impl QueueDiagnostics for DiagnosticLog {
    fn record(&self, event: QueueDiagnostic) {
        self.0.lock().unwrap().push(event);
    }
}

pub type RuntimeTool = QueueTool<Backend, UnavailableReader, RealClock, DiagnosticLog, Target, ()>;

pub struct Fixture {
    pub runtime: harness::Harness,
    pub root: Arc<Scope<Security>>,
    pub clock: Arc<RealClock>,
    pub log: Arc<DiagnosticLog>,
}
impl Fixture {
    pub async fn new() -> Self {
        let runtime = harness::Harness::new();
        let root = runtime.root().await;
        Self {
            runtime,
            root,
            clock: Arc::new(RealClock::default()),
            log: Arc::new(DiagnosticLog::default()),
        }
    }
    pub fn definitions() -> ToolDefinitionCatalog {
        ToolDefinitionCatalog::new(
            &[
                QueueToolKind::Create,
                QueueToolKind::Info,
                QueueToolKind::Push,
                QueueToolKind::Take,
            ]
            .into_iter()
            .map(|kind| {
                (
                    kind.name().into(),
                    ConfiguredToolDefinition::new(
                        format!("configured {} description", kind.name()),
                        format!(r#"{{"configured":"{}"}}"#, kind.name()),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>(),
        )
    }
    pub fn tool(&self, scope: &Arc<Scope<Security>>, kind: QueueToolKind) -> RuntimeTool {
        self.tool_with_reader(scope, kind, Arc::new(UnavailableReader))
    }
    pub fn tool_with_reader<R: AuthorizedReadService<Security = Security>>(
        &self,
        scope: &Arc<Scope<Security>>,
        kind: QueueToolKind,
        reader: Arc<R>,
    ) -> QueueTool<Backend, R, RealClock, DiagnosticLog, Target, ()> {
        QueueTool::new(
            kind,
            &Self::definitions(),
            Arc::new(Backend::new(scope.clone(), self.runtime.registry.clone())),
            reader,
            self.clock.clone(),
            self.log.clone(),
        )
        .unwrap()
    }
    pub async fn call(
        &self,
        scope: &Arc<Scope<Security>>,
        kind: QueueToolKind,
        arguments: &str,
    ) -> ToolOutput {
        self.call_cancel(scope, kind, "call", arguments, &NeverCancelled)
            .await
            .unwrap()
    }
    pub async fn call_cancel(
        &self,
        scope: &Arc<Scope<Security>>,
        kind: QueueToolKind,
        call_id: &str,
        arguments: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        self.tool(scope, kind)
            .invoke(invocation(call_id, arguments), context(cancellation))
            .await
    }
    pub async fn close(&self) {
        self.runtime.close().await;
    }
}
pub fn invocation(call_id: &str, arguments: &str) -> ToolInvocation {
    ToolInvocation {
        call_id: call_id.into(),
        arguments_json: arguments.into(),
        assistant_sequence: 1,
    }
}
pub fn context(cancellation: &dyn Cancellation) -> InvocationContext<'_, Security, ()> {
    InvocationContext::new(
        Security {
            permissions: 0,
            read_only: false,
        },
        (),
        cancellation,
    )
}
pub fn json(output: ToolOutput) -> serde_json::Value {
    serde_json::from_str(&output.text)
        .unwrap_or_else(|error| panic!("expected queue JSON, got {:?}: {error}", output.text))
}
