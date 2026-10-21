//! Scoped runtime queues behind the retained queue tools' frozen domain port.

use std::sync::Arc;

use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
    runtime::AgentQueues as QueuePort,
    security::RestrictableAccessSnapshot,
};
use parrot_runtime::{
    agent_queues::{AgentQueues, QueueError},
    children::{AgentIdentity, AgentRegistry, AgentResolver, ChildError, ChildResult, Scope},
};
use parrot_storage::queue_files as storage;
use parrot_tools::tasks_queues::{
    FailureKind, ToolFailure,
    queues::{
        CreateRequest, Direction, InfoRequest, PushRequest, QueueInfo, QueueTarget,
        QueueToolBackend, ResolvedQueue, TakeRequest, TakeResult,
    },
};

/// An explicit resource address retains the exact resolved scope and queue service.
/// It never repeats a bare-name lookup in that service's parent.
pub struct QueueOwner<S: RestrictableAccessSnapshot + 'static> {
    scope: Arc<Scope<S>>,
    queues: Arc<AgentQueues<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> Clone for QueueOwner<S> {
    fn clone(&self) -> Self {
        Self {
            scope: self.scope.clone(),
            queues: self.queues.clone(),
        }
    }
}

pub struct ScopedQueueBackend<S: RestrictableAccessSnapshot + 'static> {
    identity: Arc<AgentIdentity>,
    queues: Arc<AgentQueues<S>>,
    // Keep the resolver's weak ownership: a retained tool belongs to its scope,
    // so retaining that scope or its registry here would create an owner cycle.
    resolver: AgentResolver<S>,
}
impl<S: RestrictableAccessSnapshot + 'static> ScopedQueueBackend<S> {
    pub fn new(scope: Arc<Scope<S>>, registry: Arc<AgentRegistry<S>>) -> ChildResult<Self> {
        Ok(Self {
            queues: scope.get_service()?,
            resolver: AgentResolver::new(&scope, &registry),
            identity: scope.identity().clone(),
        })
    }
}

fn info(value: storage::QueueInfo) -> OperationResult<QueueInfo, ToolFailure> {
    let size = i32::try_from(value.size).map_err(|_| {
        OperationError::Failed(ToolFailure::new(
            FailureKind::InvalidOperation,
            "queue size exceeds the wire integer range",
        ))
    })?;
    Ok(QueueInfo {
        path: value.path.to_string_lossy().into_owned(),
        name: value.name,
        description: value.description,
        size,
        closed: value.closed,
    })
}
fn direction(value: Direction) -> storage::Direction {
    match value {
        Direction::Unspecified => storage::Direction::Unspecified,
        Direction::Front => storage::Direction::Front,
        Direction::Back => storage::Direction::Back,
    }
}
fn scope_failure(error: ChildError) -> ToolFailure {
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
    ToolFailure::new(kind, error.to_string())
}
fn queue_failure(error: QueueError) -> OperationError<ToolFailure> {
    match error {
        QueueError::Scope(error) if error.code() == "cancelled" => OperationError::Cancelled,
        QueueError::Scope(error) => OperationError::Failed(scope_failure(error)),
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

impl<S: RestrictableAccessSnapshot + 'static> QueuePort for ScopedQueueBackend<S> {
    type CreateRequest = CreateRequest;
    type CreateResult = QueueInfo;
    type InfoRequest = InfoRequest<QueueOwner<S>>;
    type InfoResult = QueueInfo;
    type PushRequest = PushRequest<QueueOwner<S>>;
    type PushResult = QueueInfo;
    type TakeRequest = TakeRequest<QueueOwner<S>>;
    type TakeResult = TakeResult;
    type Error = ToolFailure;

    fn create<'a>(
        &'a self,
        request: Self::CreateRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            info(
                self.queues
                    .create(&request.name, &request.description)
                    .map_err(queue_failure)?,
            )
        })
    }
    fn info<'a>(
        &'a self,
        request: Self::InfoRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            let value = match request.target {
                QueueTarget::Invoking(name) => self.queues.get(&name),
                QueueTarget::Local(target) => target
                    .owner
                    .queues
                    .local()
                    .get(&target.name)
                    .map_err(Into::into),
            }
            .map_err(queue_failure)?;
            info(value)
        })
    }
    fn push<'a>(
        &'a self,
        request: Self::PushRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let value = match request.target {
                QueueTarget::Invoking(name) => {
                    if let Some(items) = request.items.iter().cloned().collect::<Option<Vec<_>>>() {
                        self.queues.push(
                            &name,
                            &items,
                            direction(request.direction),
                            request.close,
                            cancellation,
                        )
                    } else {
                        // Preserve the actual store's lookup/closed/null-item
                        // validation order without inventing a tool validation.
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
                QueueTarget::Local(target) => target
                    .owner
                    .queues
                    .local()
                    .push_nullable(
                        &target.name,
                        &request.items,
                        direction(request.direction),
                        request.close,
                    )
                    .map_err(Into::into),
            }
            .map_err(queue_failure)?;
            info(value)
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
                    info: taken.info.map(info).transpose()?,
                    items: taken.items,
                }),
                // Only actual open-empty is a retryable poll result. Closed
                // empty is already an acquired result and returns immediately.
                Err(QueueError::Storage(error)) if error.kind == storage::ErrorKind::Empty => {
                    Ok(TakeResult {
                        acquired: false,
                        info: error.info.map(info).transpose()?,
                        items: Vec::new(),
                    })
                }
                Err(error) => Err(queue_failure(error)),
            }
        })
    }
}
impl<S: RestrictableAccessSnapshot + 'static> QueueToolBackend<QueueOwner<S>>
    for ScopedQueueBackend<S>
{
    fn resolve(&self, address: &str) -> Result<ResolvedQueue<QueueOwner<S>>, ToolFailure> {
        let (scope, name) = self
            .resolver
            .resolve_resource(address)
            .map_err(scope_failure)?;
        let queues = scope.get_service().map_err(scope_failure)?;
        Ok(ResolvedQueue {
            owner: QueueOwner { scope, queues },
            name,
        })
    }
    fn session_id(&self) -> &str {
        &self.identity.session.session_id
    }
    fn resolved_session_id<'a>(&self, target: &'a QueueOwner<S>) -> &'a str {
        &target.scope.identity().session.session_id
    }
}
