use super::{QueueInventory, QueueInventorySnapshot, QueueInventorySubscription};
use crate::children::{
    AgentIdentity, AgentResolver, ChildError, ChildRegistry, ChildResult, Scope, ScopedSession,
};
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, ScopeLifetime},
    security::RestrictableAccessSnapshot,
};
use parrot_foundation::paths::UserSessionResources;
use parrot_storage::queue_files::{
    self, Direction, ErrorKind, Owner, QueueInfo, QueueStore, TryTakeResult,
};
use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueError {
    Storage(queue_files::Error),
    Scope(ChildError),
}
pub type QueueResult<T> = Result<T, QueueError>;
impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(f),
            Self::Scope(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for QueueError {}
impl From<queue_files::Error> for QueueError {
    fn from(error: queue_files::Error) -> Self {
        Self::Storage(error)
    }
}
impl From<ChildError> for QueueError {
    fn from(error: ChildError) -> Self {
        Self::Scope(error)
    }
}
impl From<QueueError> for ChildError {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::Scope(error) => error,
            QueueError::Storage(error) => {
                let code = match error.kind {
                    ErrorKind::Cancelled | ErrorKind::CancelledBeforeWait => "cancelled",
                    ErrorKind::Argument => "argument",
                    ErrorKind::Io => "io",
                    ErrorKind::UnauthorizedAccess => "access_denied",
                    ErrorKind::Inventory | ErrorKind::Disposed => "invalid_operation",
                    ErrorKind::Queue
                    | ErrorKind::NotFound
                    | ErrorKind::AlreadyExists
                    | ErrorKind::Closed
                    | ErrorKind::Empty => "unexpected",
                };
                ChildError::failure(code, error.to_string())
            }
        }
    }
}
fn disposed() -> QueueError {
    queue_files::Error::new(ErrorKind::Disposed, "Cannot access a disposed object.").into()
}
fn io(error: std::io::Error) -> QueueError {
    queue_files::Error::new(
        match error.kind() {
            std::io::ErrorKind::InvalidInput => ErrorKind::Argument,
            std::io::ErrorKind::PermissionDenied => ErrorKind::UnauthorizedAccess,
            _ => ErrorKind::Io,
        },
        error.to_string(),
    )
    .into()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueDiagnostic {
    pub event: &'static str,
    pub agent_session_id: String,
    pub outcome: &'static str,
}
pub trait QueueDiagnostics: Send + Sync {
    fn record(&self, event: QueueDiagnostic);
}
pub struct QueueOwnerSnapshot {
    pub identity: Arc<AgentIdentity>,
    pub queues: Vec<QueueInfo>,
}
struct Binding<S: RestrictableAccessSnapshot + 'static> {
    owner: Weak<Scope<S>>,
    children: Weak<ChildRegistry<S>>,
    parent: Option<Arc<AgentQueues<S>>>,
}
pub struct AgentQueues<S: RestrictableAccessSnapshot + 'static> {
    identity: Arc<AgentIdentity>,
    agent_queue_root_directory: PathBuf,
    local: QueueStore,
    inventory: Arc<QueueInventory>,
    diagnostics: Arc<dyn QueueDiagnostics>,
    binding: OnceLock<Binding<S>>,
    disposed: AtomicBool,
}
impl<S: RestrictableAccessSnapshot + 'static> AgentQueues<S> {
    pub fn new<W>(
        identity: Arc<AgentIdentity>,
        resources: &UserSessionResources<W>,
        inventory: Arc<QueueInventory>,
        diagnostics: Arc<dyn QueueDiagnostics>,
    ) -> QueueResult<Arc<Self>> {
        let expected = Owner {
            session_id: identity.session.session_id.clone(),
            name: identity.session.name.clone(),
            parent_session_id: identity.session.parent_session_id.clone(),
            parent_name: identity.parent_name.clone(),
        };
        if inventory.owner() != &expected {
            return Err(
                ChildError::registry("queue inventory does not match owner identity").into(),
            );
        }
        let directory = if identity.session.depth == 0 {
            resources.queue_directory()
        } else {
            resources
                .agent_queue_directory(&identity.session.session_id)
                .map_err(io)?
        };
        Ok(Arc::new(Self {
            identity,
            agent_queue_root_directory: resources.agent_queue_root_directory(),
            local: QueueStore::new(directory)?,
            inventory,
            diagnostics,
            binding: OnceLock::new(),
            disposed: AtomicBool::new(false),
        }))
    }
    pub fn identity(&self) -> &Arc<AgentIdentity> {
        &self.identity
    }
    pub fn local(&self) -> &QueueStore {
        &self.local
    }
    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }
    pub fn initialize(&self) -> QueueResult<()> {
        if self.identity.session.depth == 0 && self.agent_queue_root_directory.exists() {
            std::fs::remove_dir_all(&self.agent_queue_root_directory).map_err(io)?;
        }
        self.local
            .attach_inventory(self.inventory.owner().clone(), self.inventory.clone())?;
        Ok(())
    }
    /// Rust composition staging: bind the exact finished scope before its admission/publication.
    pub fn bind_scope(self: &Arc<Self>, scope: &Arc<Scope<S>>) -> ChildResult<()> {
        if self.is_disposed() {
            return Err(ChildError::failure(
                "invalid_operation",
                "Cannot access a disposed object.",
            ));
        }
        if !Arc::ptr_eq(&self.identity, scope.identity())
            || !Arc::ptr_eq(self, &scope.get_service::<Self>()?)
        {
            return Err(ChildError::registry(
                "queue service does not match owning scope",
            ));
        }
        if let Some(bound) = self.binding.get() {
            return if Weak::ptr_eq(&bound.owner, &Arc::downgrade(scope)) {
                Ok(())
            } else {
                Err(ChildError::registry(
                    "queue service is already bound to a different scope",
                ))
            };
        }
        let parent = scope
            .parent_scope()
            .parent()
            .map(|parent| parent.get_service::<Self>())
            .transpose()?;
        self.binding
            .set(Binding {
                owner: Arc::downgrade(scope),
                children: Arc::downgrade(scope.children()),
                parent,
            })
            .map_err(|_| ChildError::registry("queue service is already bound"))
    }
    fn binding(&self) -> QueueResult<&Binding<S>> {
        self.binding.get().ok_or_else(|| {
            ChildError::registry("queue service is not bound to an owning scope").into()
        })
    }
    fn children(&self) -> QueueResult<Arc<ChildRegistry<S>>> {
        self.binding()?.children.upgrade().ok_or_else(|| {
            ChildError::registry("queue owning child registry is no longer available").into()
        })
    }
    /// Source Attach is deliberately a no-op apart from the disposed check.
    pub fn attach(&self, _session: &Arc<dyn ScopedSession<S>>) -> QueueResult<()> {
        if self.is_disposed() {
            Err(disposed())
        } else {
            Ok(())
        }
    }
    pub fn capture_inventory(&self) -> QueueInventorySnapshot {
        self.inventory.capture()
    }
    pub fn subscribe_inventory(&self) -> queue_files::Result<QueueInventorySubscription> {
        self.inventory.subscribe()
    }
    pub fn snapshot(&self) -> QueueResult<QueueOwnerSnapshot> {
        if self.is_disposed() {
            return Ok(QueueOwnerSnapshot {
                identity: self.identity.clone(),
                queues: Vec::new(),
            });
        }
        let children = self.children()?;
        children.with_children(|_| {
            Ok(QueueOwnerSnapshot {
                identity: self.identity.clone(),
                queues: if self.is_disposed() {
                    Vec::new()
                } else {
                    self.local.list()?
                },
            })
        })
    }
    pub fn validate_parent(&self) -> QueueResult<()> {
        if let Some(parent) = &self.binding()?.parent {
            for queue in self.local.list()? {
                parent.ensure_missing(&queue.name)?;
            }
        }
        Ok(())
    }
    pub fn create(&self, name: &str, description: &str) -> QueueResult<QueueInfo> {
        if self.is_disposed() {
            return Err(disposed());
        }
        let binding = self.binding()?;
        if binding
            .parent
            .as_ref()
            .is_some_and(|parent| parent.is_disposed())
        {
            return Err(disposed());
        }
        let own_children = self.children()?;
        let create = || {
            own_children.with_children(|children| {
                if self.is_disposed()
                    || binding
                        .parent
                        .as_ref()
                        .is_some_and(|parent| parent.is_disposed())
                {
                    return Err(disposed());
                }
                if let Some(parent) = &binding.parent {
                    parent.ensure_missing(name)?;
                }
                for child in children {
                    child.get_service::<Self>()?.ensure_missing(name)?;
                }
                self.local.create(name, description).map_err(Into::into)
            })
        };
        let result = if let Some(parent) = &binding.parent {
            let parent_children = parent.children()?;
            parent_children.with_children(|_| create())
        } else {
            create()
        }?;
        self.diagnostics.record(QueueDiagnostic {
            event: "created",
            agent_session_id: self.identity.session.session_id.clone(),
            outcome: "created",
        });
        Ok(result)
    }
    fn resolve(&self, name: &str) -> QueueResult<QueueStore> {
        match self.local.get(name) {
            Ok(_) => return Ok(self.local.clone()),
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(parent) = &self.binding()?.parent {
            match parent.local.get(name) {
                Ok(_) => return Ok(parent.local.clone()),
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(queue_files::Error::new(
            ErrorKind::NotFound,
            format!("queue: '{name}' was not found"),
        )
        .into())
    }
    /// Explicit resource addresses select only that owner's Local store, never its parent.
    pub fn resolve_address(
        self: &Arc<Self>,
        resolver: &AgentResolver<S>,
        name: &str,
    ) -> QueueResult<(QueueStore, String)> {
        if name.contains('/') {
            let (scope, name) = resolver.resolve_resource(name)?;
            Ok((scope.get_service::<Self>()?.local.clone(), name))
        } else {
            Ok((self.resolve(name)?, name.to_owned()))
        }
    }
    pub fn get(&self, name: &str) -> QueueResult<QueueInfo> {
        Ok(self.resolve(name)?.get(name)?)
    }
    pub fn list(&self) -> QueueResult<Vec<QueueInfo>> {
        let mut own = self.local.list()?;
        if let Some(parent) = &self.binding()?.parent {
            own.extend(parent.local.list()?);
            own.sort_by(|left, right| left.name.cmp(&right.name));
        }
        Ok(own)
    }
    pub fn push(
        &self,
        name: &str,
        items: &[String],
        direction: Direction,
        close: bool,
        cancellation: &dyn Cancellation,
    ) -> QueueResult<QueueInfo> {
        if cancellation.is_cancelled() {
            return Err(queue_files::Error::new(
                ErrorKind::Cancelled,
                "The operation was canceled.",
            )
            .into());
        }
        let result = self.resolve(name)?.push(name, items, direction, close)?;
        if close {
            self.diagnostics.record(QueueDiagnostic {
                event: "closed",
                agent_session_id: self.identity.session.session_id.clone(),
                outcome: "closed",
            });
        }
        Ok(result)
    }
    pub fn try_take(
        &self,
        name: &str,
        count: i32,
        direction: Direction,
    ) -> QueueResult<TryTakeResult> {
        Ok(self.resolve(name)?.try_take(name, count, direction)?)
    }
    pub fn ensure_missing(&self, name: &str) -> QueueResult<()> {
        if self.is_disposed() {
            return Ok(());
        }
        match self.local.get(name) {
            Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => Err(queue_files::Error::new(
                ErrorKind::AlreadyExists,
                format!("queue: '{name}' already exists"),
            )
            .into()),
        }
    }
    pub fn dispose(&self) -> QueueResult<()> {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let dispose = || -> QueueResult<()> {
            self.local.dispose()?;
            self.inventory.dispose();
            Ok(())
        };
        if let Some(binding) = self.binding.get() {
            let own_children = binding.children.upgrade();
            let dispose_own = || match &own_children {
                Some(children) => children.with_children(|_| dispose()),
                None => dispose(),
            };
            if let Some(parent) = &binding.parent {
                // Retain the real gate even if only its registry, not its scope,
                // remains alive. Weak storage avoids the parent/child queue cycle.
                match parent.binding()?.children.upgrade() {
                    Some(children) => children.with_children(|_| dispose_own())?,
                    None if parent.is_disposed() => dispose_own()?,
                    None => {
                        return Err(ChildError::registry(
                            "queue parent child registry is no longer available",
                        )
                        .into());
                    }
                }
            } else {
                dispose_own()?;
            }
        } else {
            dispose()?;
        }
        if self.identity.session.depth > 0 && self.local.directory().exists() {
            std::fs::remove_dir_all(self.local.directory()).map_err(io)?;
        }
        Ok(())
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ScopeLifetime for AgentQueues<S> {
    type Error = ChildError;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move { self.dispose().map_err(Into::into) })
    }
}
pub struct QueueChildAdmissionValidator;
impl QueueChildAdmissionValidator {
    /// Called by the real ChildRegistry under its parent's admission gate.
    pub fn validate<S: RestrictableAccessSnapshot + 'static>(
        scope: &Arc<Scope<S>>,
    ) -> ChildResult<()> {
        let queues = scope.get_service::<AgentQueues<S>>()?;
        queues.bind_scope(scope)?;
        queues.validate_parent().map_err(Into::into)
    }
}
