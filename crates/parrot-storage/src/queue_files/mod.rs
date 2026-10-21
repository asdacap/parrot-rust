//! Selected-session JSONL queue persistence and inventory attachment.
//!
//! Inventory callbacks are the storage boundary. Revisioned inventory feeds,
//! parent/child queue routing and waiting for future items belong to runtime.
//!
//! Whole null arguments are unrepresentable in the typed Rust API, not turned
//! into fabricated runtime exceptions. Nullable persisted data and nullable item
//! entries remain runtime-validated by the implementation.
//!
//! ```compile_fail
//! use parrot_storage::queue_files::QueueStore;
//! QueueStore::new(None::<&std::path::Path>);
//! ```
//! ```compile_fail
//! use parrot_storage::queue_files::QueueStore;
//! fn null_name(store: &QueueStore) { store.get(None); }
//! ```
//! ```compile_fail
//! use parrot_storage::queue_files::QueueStore;
//! fn null_description(store: &QueueStore) { store.create("work", None); }
//! ```
//! ```compile_fail
//! use parrot_storage::queue_files::{Direction, QueueStore};
//! fn null_items(store: &QueueStore) { store.push("work", None, Direction::Back, false); }
//! ```
//! ```compile_fail
//! use std::sync::Arc;
//! use parrot_storage::queue_files::{Inventory, QueueStore};
//! fn null_owner(store: &QueueStore, inventory: Arc<dyn Inventory>) {
//!     store.attach_inventory(None, inventory);
//! }
//! ```
//! ```compile_fail
//! use parrot_storage::queue_files::{Owner, QueueStore};
//! fn null_inventory(store: &QueueStore, owner: Owner) { store.attach_inventory(owner, None); }
//! ```

mod format;
mod gate;
#[cfg(all(test, unix))]
mod publication_protocol_tests;

use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::paths::SelectedSessionRoot;

pub const MAXIMUM_FILE_BYTES: usize = 16 << 20;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Queue,
    NotFound,
    AlreadyExists,
    Closed,
    Empty,
    Disposed,
    Cancelled,
    CancelledBeforeWait,
    Inventory,
    Io,
    UnauthorizedAccess,
    Argument,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub info: Option<QueueInfo>,
    pub inner: Option<Box<Error>>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            info: None,
            inner: None,
        }
    }

    fn with_inner(mut self, inner: Error) -> Self {
        self.inner = Some(Box::new(inner));
        self
    }

    fn queue(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Queue, message)
    }
    fn disposed() -> Self {
        Self::new(ErrorKind::Disposed, "Cannot access a disposed object.")
    }
    fn not_found(name: &str) -> Self {
        Self::new(
            ErrorKind::NotFound,
            format!("queue: '{name}' was not found"),
        )
    }
    fn too_large() -> Self {
        Self::queue(format!("queue: file exceeds {MAXIMUM_FILE_BYTES} bytes"))
    }
    fn io(message: &str, error: std::io::Error) -> Self {
        let kind = if error.kind() == std::io::ErrorKind::PermissionDenied {
            ErrorKind::UnauthorizedAccess
        } else {
            ErrorKind::Io
        };
        Self::queue(message).with_inner(Self::new(kind, error.to_string()))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner
            .as_deref()
            .map(|error| error as &dyn std::error::Error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Unspecified,
    Front,
    Back,
    Invalid(i32),
}

impl Direction {
    fn resolve(self, default: Self) -> Result<Self> {
        match self {
            Self::Unspecified => Ok(default),
            Self::Front | Self::Back => Ok(self),
            Self::Invalid(value) => Err(Error::queue(format!(
                "queue: direction '{value}' must be front or back"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueInfo {
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub size: usize,
    pub closed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeResult {
    pub items: Vec<String>,
    pub info: QueueInfo,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TryTakeResult {
    pub acquired: bool,
    pub items: Vec<String>,
    pub info: Option<QueueInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Owner {
    pub session_id: String,
    pub name: String,
    pub parent_session_id: String,
    pub parent_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueState {
    pub owner: Owner,
    pub name: String,
    pub description: String,
    pub item_count: usize,
}

/// Storage callback interface, not the runtime inventory/feed implementation.
/// Calls to update are serialized with queue mutations. Register/unregister run
/// outside the queue gate to preserve source attachment/disposal reentrancy.
pub trait Inventory: Send + Sync {
    fn register_owner(&self, queues: &[QueueState]) -> Result<()>;
    fn update(&self, state: QueueState) -> Result<()>;
    fn unregister_owner(&self) -> Result<()>;
}

struct Metadata {
    name: String,
    description: Option<String>,
    closed: bool,
}
impl Metadata {
    fn info(&self, path: PathBuf, size: usize) -> QueueInfo {
        QueueInfo {
            path,
            name: self.name.clone(),
            description: self.description.clone().unwrap_or_default(),
            size,
            closed: self.closed,
        }
    }
}

#[derive(Default)]
struct State {
    disposed: bool,
    owner: Option<Owner>,
    inventory: Option<Arc<dyn Inventory>>,
    attaching: bool,
    pending: Vec<QueueState>,
}

struct Inner {
    directory: PathBuf,
    gate: Arc<gate::Gate>,
    state: Mutex<State>,
}

/// A clone is an observer/handle to the same store; explicit dispose closes all
/// handles. Arc ownership retains gate resources through an in-flight attach.
#[derive(Clone)]
pub struct QueueStore {
    inner: Arc<Inner>,
}

impl QueueStore {
    /// Bind an internal queue directory to an already selected session root.
    pub fn for_session(root: &SelectedSessionRoot, relative: impl AsRef<Path>) -> Result<Self> {
        let address = root
            .resolve_internal(relative)
            .map_err(|error| Error::queue(error.to_string()))?;
        Self::new(address.path())
    }

    /// The path is supplied by session composition, never discovered in another root.
    pub fn new(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if directory.as_os_str().is_empty() {
            return Err(Error::new(ErrorKind::Argument, "directory cannot be empty"));
        }
        if !directory.is_absolute() {
            return Err(Error::queue("queue: absolute directory is required"));
        }
        let mut resolved = PathBuf::new();
        for component in directory.components() {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
        }
        fs::create_dir_all(&resolved)
            .map_err(|error| Error::io("queue: path is not a directory", error))?;
        if !resolved.is_dir() {
            return Err(Error::queue("queue: path is not a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&resolved, fs::Permissions::from_mode(0o700))
                .map_err(|error| Error::io("queue: path is not a directory", error))?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                directory: resolved,
                gate: Arc::default(),
                state: Mutex::default(),
            }),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    fn resolve(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.directory().join(format!("{name}.jsonl")))
    }

    fn check_disposed(&self) -> Result<()> {
        if self
            .inner
            .state
            .lock()
            .expect("queue state poisoned")
            .disposed
        {
            Err(Error::disposed())
        } else {
            Ok(())
        }
    }

    pub fn create(&self, name: &str, description: &str) -> Result<QueueInfo> {
        let path = self.resolve(name)?;
        let _permit = self.inner.gate.acquire(false)?;
        self.check_disposed()?;
        let metadata = Metadata {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            closed: false,
        };
        format::write(&path, &metadata, &[], true)?;
        // Source Create does not publish an empty inventory entry.
        Ok(metadata.info(path, 0))
    }

    pub fn push(
        &self,
        name: &str,
        items: &[String],
        direction: Direction,
        close: bool,
    ) -> Result<QueueInfo> {
        self.push_nullable(
            name,
            &items.iter().cloned().map(Some).collect::<Vec<_>>(),
            direction,
            close,
        )
    }

    /// Nullable entries preserve the source null-item validation boundary.
    pub fn push_nullable(
        &self,
        name: &str,
        items: &[Option<String>],
        direction: Direction,
        close: bool,
    ) -> Result<QueueInfo> {
        let direction = direction.resolve(Direction::Back)?;
        let path = self.resolve(name)?;
        let _permit = self.inner.gate.acquire(false)?;
        self.check_disposed()?;
        let (mut metadata, mut stored) = format::read(&path, name)?;
        if metadata.closed {
            if close && items.is_empty() {
                return Ok(metadata.info(path, stored.len()));
            }
            return Err(Error::new(
                ErrorKind::Closed,
                format!("queue: '{name}' is closed"),
            ));
        }
        let mut added: Vec<String> = items
            .iter()
            .map(|item| {
                item.clone()
                    .ok_or_else(|| Error::queue("queue: items cannot contain null"))
            })
            .collect::<Result<_>>()?;
        if direction == Direction::Front && !added.is_empty() {
            added.reverse();
            added.extend(stored);
            stored = added;
        } else {
            stored.extend(added);
        }
        metadata.closed |= close;
        format::write(&path, &metadata, &stored, false)?;
        let info = metadata.info(path, stored.len());
        self.publish(&info)?;
        Ok(info)
    }

    pub async fn take(
        &self,
        name: &str,
        count: i32,
        direction: Direction,
        cancellation: &dyn Cancellation,
    ) -> Result<TakeResult> {
        let direction = take_direction(count, direction)?;
        let path = self.resolve(name)?;
        let permit = self.inner.gate.acquire_async(cancellation).await?;
        let store = self.clone();
        let name = name.to_owned();
        // Once the source semaphore is acquired cancellation is no longer read.
        // Blocking file work is owned by the worker, not the observation future.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            store.check_disposed()?;
            let taken = take_locked(&path, &name, count as usize, direction)?;
            store.publish(&taken.info)?;
            Ok(taken)
        })
        .await
        .map_err(|error| Error::queue(format!("queue worker failed: {error}")))?
    }

    pub fn try_take(&self, name: &str, count: i32, direction: Direction) -> Result<TryTakeResult> {
        let direction = take_direction(count, direction)?;
        let path = self.resolve(name)?;
        let Some(_permit) = self.inner.gate.try_acquire()? else {
            return Ok(TryTakeResult {
                acquired: false,
                items: Vec::new(),
                info: None,
            });
        };
        self.check_disposed()?;
        let taken = take_locked(&path, name, count as usize, direction)?;
        self.publish(&taken.info)?;
        Ok(TryTakeResult {
            acquired: true,
            items: taken.items,
            info: Some(taken.info),
        })
    }

    pub fn get(&self, name: &str) -> Result<QueueInfo> {
        let path = self.resolve(name)?;
        let _permit = self.inner.gate.acquire(false)?;
        self.check_disposed()?;
        let (metadata, items) = format::read(&path, name)?;
        Ok(metadata.info(path, items.len()))
    }

    pub fn list(&self) -> Result<Vec<QueueInfo>> {
        let _permit = self.inner.gate.acquire(false)?;
        self.check_disposed()?;
        self.list_locked(true)
    }

    fn list_locked(&self, wrap_errors: bool) -> Result<Vec<QueueInfo>> {
        if !self.directory().exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(self.directory())
            .map_err(|error| Error::io("queue: could not read directory", error))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| Error::io("queue: could not read directory", error))?;
        paths.retain(|path| {
            fs::symlink_metadata(path).is_ok_and(|info| !info.is_dir())
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
        });
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                validate_name(&name)?;
                let (metadata, items) = format::read(&path, &name).map_err(|error| {
                    if wrap_errors {
                        Error::queue(format!("queue: could not read '{name}'")).with_inner(error)
                    } else {
                        error
                    }
                })?;
                Ok(metadata.info(path, items.len()))
            })
            .collect()
    }

    fn publish(&self, info: &QueueInfo) -> Result<()> {
        let mut state = self.inner.state.lock().expect("queue state poisoned");
        let Some(owner) = state.owner.clone() else {
            return Ok(());
        };
        let update = QueueState {
            owner,
            name: info.name.clone(),
            description: info.description.clone(),
            item_count: info.size,
        };
        if let Some(inventory) = state.inventory.clone() {
            drop(state);
            inventory.update(update)
        } else {
            if state.attaching {
                if let Some(pending) = state
                    .pending
                    .iter_mut()
                    .find(|pending| pending.name == update.name)
                {
                    *pending = update;
                } else {
                    state.pending.push(update);
                }
            }
            Ok(())
        }
    }

    pub fn attach_inventory(&self, owner: Owner, inventory: Arc<dyn Inventory>) -> Result<()> {
        // Arc retains the native gate throughout callbacks, including disposal.
        let states;
        {
            let _permit = self.inner.gate.acquire(false)?;
            self.check_disposed()?;
            {
                let state = self.inner.state.lock().expect("queue state poisoned");
                if state.inventory.is_some() || state.attaching {
                    return Err(Error::new(
                        ErrorKind::Inventory,
                        "Queue store inventory is already attached.",
                    ));
                }
            }
            states = self
                .list_locked(false)?
                .into_iter()
                .filter(|info| info.size > 0)
                .map(|info| QueueState {
                    owner: owner.clone(),
                    name: info.name,
                    description: info.description,
                    item_count: info.size,
                })
                .collect::<Vec<_>>();
            let mut state = self.inner.state.lock().expect("queue state poisoned");
            state.owner = Some(owner);
            state.attaching = true;
        }
        if let Err(error) = inventory.register_owner(&states) {
            let _permit = self.inner.gate.acquire(true)?;
            let mut state = self.inner.state.lock().expect("queue state poisoned");
            state.pending.clear();
            state.owner = None;
            state.attaching = false;
            return Err(error);
        }
        loop {
            let pending;
            let disposed;
            {
                let _permit = self.inner.gate.acquire(true)?;
                let mut state = self.inner.state.lock().expect("queue state poisoned");
                disposed = state.disposed;
                if disposed {
                    state.pending.clear();
                    state.attaching = false;
                    pending = Vec::new();
                } else if state.pending.is_empty() {
                    state.inventory = Some(inventory.clone());
                    state.attaching = false;
                    return Ok(());
                } else {
                    pending = std::mem::take(&mut state.pending);
                }
            }
            if disposed {
                inventory.unregister_owner()?;
                return Err(Error::disposed());
            }
            for update in pending {
                inventory.update(update)?;
            }
        }
    }

    pub fn dispose(&self) -> Result<()> {
        let inventory;
        {
            let _permit = self.inner.gate.acquire(false)?;
            let mut state = self.inner.state.lock().expect("queue state poisoned");
            if state.disposed {
                return Ok(());
            }
            state.disposed = true;
            inventory = if state.owner.is_some() {
                state.inventory.take()
            } else {
                None
            };
            state.owner = None;
            state.pending.clear();
        }
        if let Some(inventory) = inventory {
            inventory.unregister_owner()?;
        }
        self.inner.gate.dispose();
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.split('-').any(|word| {
            word.is_empty()
                || !word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    {
        return Err(Error::queue(format!(
            "queue: name '{name}' must contain only lowercase ASCII alphanumeric words separated by hyphens"
        )));
    }
    Ok(())
}

fn take_direction(count: i32, direction: Direction) -> Result<Direction> {
    if count <= 0 {
        return Err(Error::queue(format!(
            "queue: count {count} must be positive"
        )));
    }
    direction.resolve(Direction::Front)
}

fn take_locked(path: &Path, name: &str, count: usize, direction: Direction) -> Result<TakeResult> {
    let (metadata, mut items) = format::read(path, name)?;
    if items.is_empty() {
        let info = metadata.info(path.to_path_buf(), 0);
        if metadata.closed {
            return Ok(TakeResult {
                items: Vec::new(),
                info,
            });
        }
        let mut error = Error::new(ErrorKind::Empty, format!("queue: '{name}' is empty"));
        error.info = Some(info);
        return Err(error);
    }
    let count = count.min(items.len());
    let taken = if direction == Direction::Front {
        items.drain(..count).collect()
    } else {
        items.drain(items.len() - count..).rev().collect()
    };
    format::write(path, &metadata, &items, false)?;
    Ok(TakeResult {
        items: taken,
        info: metadata.info(path.to_path_buf(), items.len()),
    })
}
