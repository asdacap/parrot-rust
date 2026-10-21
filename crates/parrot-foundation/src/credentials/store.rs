use super::model::{CURRENT_VERSION, Credential, CredentialError};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
};

pub const MAX_STORE_BYTES: u64 = 16 << 20;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();

type Lease = Arc<tokio::sync::OwnedMutexGuard<()>>;

fn path_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let registry = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

async fn admission(
    lock: Arc<tokio::sync::Mutex<()>>,
    cancellation: &dyn Cancellation,
) -> OperationResult<Lease, CredentialError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(OperationError::Cancelled),
        guard = lock.lock_owned() => Ok(Arc::new(guard)),
    }
}

async fn blocking<T, F>(lease: Lease, work: F) -> Result<T, CredentialError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CredentialError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        work()
    })
    .await
    .unwrap_or(Err(CredentialError::Malformed))
}

/// Cancellation waits for an already-running filesystem job to settle; blocking syscalls cannot be interrupted.
async fn cancelable_blocking<T, F>(
    lease: Lease,
    cancellation: &dyn Cancellation,
    work: F,
) -> OperationResult<T, CredentialError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CredentialError> + Send + 'static,
{
    if cancellation.is_cancelled() {
        let _ = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            drop(work);
        })
        .await;
        return Err(OperationError::Cancelled);
    }
    let worker_lease = lease.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        let _lease = worker_lease;
        work()
    });
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            if let Ok(Ok(value)) = (&mut job).await {
                let cleanup_lease = lease;
                let _ = tokio::task::spawn_blocking(move || {
                    let _lease = cleanup_lease;
                    drop(value);
                }).await;
            }
            Err(OperationError::Cancelled)
        }
        result = &mut job => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(OperationError::Failed(error)),
            Err(_) => Err(OperationError::Failed(CredentialError::Malformed)),
        }
    }
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum WritePhase {
    Write,
    Flush,
    Commit,
}

#[cfg(test)]
type WritePhaseHook = Arc<dyn Fn(WritePhase) + Send + Sync>;

struct PreparedWrite {
    #[cfg(test)]
    phase_hook: Option<WritePhaseHook>,
    file: Option<std::fs::File>,
    temporary: TemporaryFile,
    target: PathBuf,
    parent: PathBuf,
    output: Option<Vec<u8>>,
}

#[derive(Default)]
struct OrderedEntries(Vec<(String, Box<RawValue>)>);

impl OrderedEntries {
    fn insert(&mut self, name: String, value: Box<RawValue>) {
        if let Some((_, existing)) = self.0.iter_mut().find(|(key, _)| key == &name) {
            *existing = value;
        } else {
            self.0.push((name, value));
        }
    }

    fn remove(&mut self, name: &str) -> Option<Box<RawValue>> {
        let index = self.0.iter().position(|(key, _)| key == name)?;
        Some(self.0.remove(index).1)
    }

    fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(name, _)| name)
    }
}

impl Serialize for OrderedEntries {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for OrderedEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = OrderedEntries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a credential name-to-value object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = OrderedEntries::default();
                while let Some((name, value)) = map.next_entry()? {
                    entries.insert(name, value);
                }
                Ok(entries)
            }
        }

        deserializer.deserialize_map(EntriesVisitor)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    version: i32,
    credentials: OrderedEntries,
}

pub struct CredentialStore {
    #[cfg(test)]
    phase_hook: Option<WritePhaseHook>,
    path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl CredentialStore {
    /// Stores credentials only at this explicitly selected configuration file.
    /// This type does not discover old roots or perform path fallback.
    pub fn at_selected_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        Self {
            #[cfg(test)]
            phase_hook: None,
            lock: path_lock(&path),
            path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, CredentialError>> {
        Box::pin(async move {
            let lease = admission(self.lock.clone(), cancellation).await?;
            let path = self.path.clone();
            let opened = blocking(lease.clone(), move || {
                match OpenOptions::new().read(true).open(path) {
                    Ok(file) => {
                        let metadata = file
                            .metadata()
                            .map_err(|_| CredentialError::MalformedStore)?;
                        Ok(Some((file, metadata.len())))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(_) => Err(CredentialError::MalformedStore),
                }
            })
            .await
            .map_err(OperationError::Failed)?;
            let Some((file, len)) = opened else {
                return Ok(None);
            };
            if len > MAX_STORE_BYTES {
                return Err(OperationError::Failed(CredentialError::StoreTooLarge));
            }
            let bytes = cancelable_blocking(lease, cancellation, move || {
                let mut bytes = Vec::with_capacity(len as usize);
                file.take(MAX_STORE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| CredentialError::MalformedStore)?;
                if bytes.len() as u64 > MAX_STORE_BYTES {
                    return Err(CredentialError::StoreTooLarge);
                }
                if bytes.is_empty() {
                    return Err(CredentialError::MalformedStore);
                }
                Ok(bytes)
            })
            .await?;
            let parsed = parse_store(&bytes).map_err(OperationError::Failed)?;
            let mut entries = parsed;
            let Some(value) = entries.remove(name) else {
                return Ok(None);
            };
            parse_entry(value.get())
                .map_err(OperationError::Failed)?
                .map(Some)
                .ok_or(OperationError::Failed(CredentialError::Malformed))
        })
    }

    pub fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), CredentialError>> {
        Box::pin(async move {
            if name.is_empty() {
                return Err(OperationError::Failed(CredentialError::Malformed));
            }
            credential.validate().map_err(OperationError::Failed)?;
            let encoded = serde_json::to_string(credential)
                .map_err(|_| OperationError::Failed(CredentialError::Malformed))?;
            let raw = RawValue::from_string(encoded)
                .map_err(|_| OperationError::Failed(CredentialError::Malformed))?;
            let lease = admission(self.lock.clone(), cancellation).await?;
            let entries = self.read_entries(lease.clone(), cancellation).await?;
            let mut entries = entries;
            entries.insert(name.to_owned(), raw);
            self.write_entries(entries, lease, cancellation).await
        })
    }

    pub fn delete<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), CredentialError>> {
        Box::pin(async move {
            let lease = admission(self.lock.clone(), cancellation).await?;
            let mut entries = self.read_entries(lease.clone(), cancellation).await?;
            if entries.remove(name).is_none() {
                return Err(OperationError::Failed(CredentialError::NotFound));
            }
            self.write_entries(entries, lease, cancellation).await
        })
    }

    pub fn list<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<String>, CredentialError>> {
        Box::pin(async move {
            let lease = admission(self.lock.clone(), cancellation).await?;
            let entries = self.read_entries(lease, cancellation).await?;
            let mut names: Vec<_> = entries.keys().cloned().collect();
            names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Ok(names)
        })
    }

    pub(crate) fn snapshot<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BTreeMap<String, Credential>, CredentialError>> {
        Box::pin(async move {
            let names = self.list(cancellation).await?;
            let mut entries = BTreeMap::new();
            for name in names {
                if let Some(value) = self.get(&name, cancellation).await? {
                    entries.insert(name, value);
                }
            }
            Ok(entries)
        })
    }

    pub(crate) fn replace_snapshot<'a>(
        &'a self,
        entries: BTreeMap<String, Credential>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), CredentialError>> {
        Box::pin(async move {
            let existing = self.list(cancellation).await?;
            for name in existing {
                if !entries.contains_key(&name) {
                    self.delete(&name, cancellation).await?;
                }
            }
            for (name, credential) in entries {
                self.set(&name, &credential, cancellation).await?;
            }
            Ok(())
        })
    }

    async fn read_entries(
        &self,
        lease: Lease,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<OrderedEntries, CredentialError> {
        let path = self.path.clone();
        let opened = blocking(lease.clone(), move || {
            match OpenOptions::new().read(true).open(path) {
                Ok(file) => {
                    let metadata = file
                        .metadata()
                        .map_err(|_| CredentialError::MalformedStore)?;
                    Ok(Some((file, metadata.len())))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(CredentialError::MalformedStore),
            }
        })
        .await
        .map_err(OperationError::Failed)?;
        let Some((file, len)) = opened else {
            return Ok(OrderedEntries::default());
        };
        if len > MAX_STORE_BYTES {
            return Err(OperationError::Failed(CredentialError::StoreTooLarge));
        }
        let bytes = cancelable_blocking(lease, cancellation, move || {
            let mut bytes = Vec::with_capacity(len as usize);
            file.take(MAX_STORE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| CredentialError::MalformedStore)?;
            if bytes.len() as u64 > MAX_STORE_BYTES {
                return Err(CredentialError::StoreTooLarge);
            }
            if bytes.is_empty() {
                return Err(CredentialError::MalformedStore);
            }
            Ok(bytes)
        })
        .await?;
        parse_store(&bytes).map_err(OperationError::Failed)
    }

    async fn write_entries(
        &self,
        entries: OrderedEntries,
        lease: Lease,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<(), CredentialError> {
        let path = self.path.clone();
        let prepared = blocking(lease.clone(), move || prepare_write(path, entries))
            .await
            .map_err(OperationError::Failed)?;
        #[cfg(test)]
        let prepared = PreparedWrite {
            phase_hook: self.phase_hook.clone(),
            ..prepared
        };
        let mut prepared = cancelable_blocking(lease.clone(), cancellation, move || {
            let mut prepared = prepared;
            #[cfg(test)]
            if let Some(hook) = &prepared.phase_hook {
                hook(WritePhase::Write);
            }
            let mut file = prepared.file.take().ok_or(CredentialError::Malformed)?;
            let output = prepared.output.take().ok_or(CredentialError::Malformed)?;
            file.write_all(&output)
                .map_err(|_| CredentialError::Malformed)?;
            prepared.file = Some(file);
            Ok(prepared)
        })
        .await?;
        prepared = cancelable_blocking(lease.clone(), cancellation, move || {
            let mut prepared = prepared;
            #[cfg(test)]
            if let Some(hook) = &prepared.phase_hook {
                hook(WritePhase::Flush);
            }
            let file = prepared.file.as_mut().ok_or(CredentialError::Malformed)?;
            file.sync_all().map_err(|_| CredentialError::Malformed)?;
            Ok(prepared)
        })
        .await?;
        blocking(lease, move || commit_write(prepared))
            .await
            .map_err(OperationError::Failed)
    }
}

fn parse_store(bytes: &[u8]) -> Result<OrderedEntries, CredentialError> {
    // RawValue bypasses serde's recursion counter. Enforce the source's whole-file
    // depth limit before retaining opaque entries or passing them to the formatter.
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 64 {
                        return Err(CredentialError::MalformedStore);
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    let parsed: StoreFile =
        serde_json::from_slice(bytes).map_err(|_| CredentialError::MalformedStore)?;
    if parsed.version != CURRENT_VERSION {
        return Err(CredentialError::MalformedStore);
    }
    Ok(parsed.credentials)
}

fn prepare_write(path: PathBuf, entries: OrderedEntries) -> Result<PreparedWrite, CredentialError> {
    let parent = path
        .parent()
        .ok_or(CredentialError::Malformed)?
        .to_path_buf();
    create_private_directory(&parent)?;
    let entries = entries
        .0
        .into_iter()
        .map(|(name, raw)| {
            indented_json(raw.get()).and_then(|formatted| {
                RawValue::from_string(formatted)
                    .map(|value| (name, value))
                    .map_err(|_| CredentialError::Malformed)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let output = serde_json::to_vec_pretty(&StoreFile {
        version: CURRENT_VERSION,
        credentials: OrderedEntries(entries),
    })
    .map_err(|_| CredentialError::Malformed)?;
    if output.len() as u64 > MAX_STORE_BYTES {
        return Err(CredentialError::StoreTooLarge);
    }
    let (temporary, file) = loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".credentials-{}-{id}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(FILE_MODE);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(CredentialError::Malformed),
        }
    };
    let cleanup = TemporaryFile(temporary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
            .map_err(|_| CredentialError::Malformed)?;
    }
    Ok(PreparedWrite {
        #[cfg(test)]
        phase_hook: None,
        file: Some(file),
        temporary: cleanup,
        target: path,
        parent,
        output: Some(output),
    })
}

fn commit_write(mut prepared: PreparedWrite) -> Result<(), CredentialError> {
    #[cfg(test)]
    if let Some(hook) = &prepared.phase_hook {
        hook(WritePhase::Commit);
    }
    drop(prepared.file.take());
    fs::rename(&prepared.temporary.0, &prepared.target).map_err(|_| CredentialError::Malformed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&prepared.target, fs::Permissions::from_mode(FILE_MODE))
            .map_err(|_| CredentialError::Malformed)?;
    }
    if let Ok(directory) = OpenOptions::new().read(true).open(&prepared.parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), CredentialError> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_private_directory(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(DIRECTORY_MODE);
        match builder.create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
                Ok(())
            }
            Err(_) => Err(CredentialError::Malformed),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|_| CredentialError::Malformed)
    }
}

fn indented_json(raw: &str) -> Result<String, CredentialError> {
    fn whitespace(bytes: &[u8], cursor: &mut usize) {
        while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
    }
    fn string_end(bytes: &[u8], cursor: &mut usize) -> Result<(), CredentialError> {
        if bytes.get(*cursor) != Some(&b'"') {
            return Err(CredentialError::Malformed);
        }
        *cursor += 1;
        while let Some(value) = bytes.get(*cursor) {
            match value {
                b'\\' => *cursor += 2,
                b'"' => {
                    *cursor += 1;
                    return Ok(());
                }
                _ => *cursor += 1,
            }
        }
        Err(CredentialError::Malformed)
    }
    fn value(
        input: &str,
        bytes: &[u8],
        cursor: &mut usize,
        depth: usize,
        output: &mut String,
    ) -> Result<(), CredentialError> {
        whitespace(bytes, cursor);
        match bytes.get(*cursor) {
            Some(b'{') => {
                *cursor += 1;
                whitespace(bytes, cursor);
                output.push('{');
                if bytes.get(*cursor) == Some(&b'}') {
                    *cursor += 1;
                    output.push('}');
                    return Ok(());
                }
                output.push('\n');
                loop {
                    whitespace(bytes, cursor);
                    output.push_str(&"  ".repeat(depth + 1));
                    let start = *cursor;
                    string_end(bytes, cursor)?;
                    output.push_str(&input[start..*cursor]);
                    whitespace(bytes, cursor);
                    if bytes.get(*cursor) != Some(&b':') {
                        return Err(CredentialError::Malformed);
                    }
                    *cursor += 1;
                    output.push_str(": ");
                    value(input, bytes, cursor, depth + 1, output)?;
                    whitespace(bytes, cursor);
                    match bytes.get(*cursor) {
                        Some(b',') => {
                            *cursor += 1;
                            output.push_str(",\n");
                        }
                        Some(b'}') => {
                            *cursor += 1;
                            output.push('\n');
                            output.push_str(&"  ".repeat(depth));
                            output.push('}');
                            return Ok(());
                        }
                        _ => return Err(CredentialError::Malformed),
                    }
                }
            }
            Some(b'[') => {
                *cursor += 1;
                whitespace(bytes, cursor);
                output.push('[');
                if bytes.get(*cursor) == Some(&b']') {
                    *cursor += 1;
                    output.push(']');
                    return Ok(());
                }
                output.push('\n');
                loop {
                    output.push_str(&"  ".repeat(depth + 1));
                    value(input, bytes, cursor, depth + 1, output)?;
                    whitespace(bytes, cursor);
                    match bytes.get(*cursor) {
                        Some(b',') => {
                            *cursor += 1;
                            output.push_str(",\n");
                        }
                        Some(b']') => {
                            *cursor += 1;
                            output.push('\n');
                            output.push_str(&"  ".repeat(depth));
                            output.push(']');
                            return Ok(());
                        }
                        _ => return Err(CredentialError::Malformed),
                    }
                }
            }
            Some(b'"') => {
                let start = *cursor;
                string_end(bytes, cursor)?;
                output.push_str(&input[start..*cursor]);
                Ok(())
            }
            Some(_) => {
                let start = *cursor;
                while bytes
                    .get(*cursor)
                    .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, b',' | b']' | b'}'))
                {
                    *cursor += 1;
                }
                if start == *cursor {
                    return Err(CredentialError::Malformed);
                }
                output.push_str(&input[start..*cursor]);
                Ok(())
            }
            None => Err(CredentialError::Malformed),
        }
    }
    let bytes = raw.as_bytes();
    let mut cursor = 0;
    let mut output = String::new();
    // The raw value is embedded two levels below the store root (the root object
    // and `credentials` mapping). Include those indentation levels before the
    // surrounding serde serializer inserts the original first line.
    value(raw, bytes, &mut cursor, 2, &mut output)?;
    whitespace(bytes, &mut cursor);
    if cursor != bytes.len() {
        return Err(CredentialError::Malformed);
    }
    Ok(output)
}

fn parse_entry(raw: &str) -> Result<Option<Credential>, CredentialError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| CredentialError::Malformed)?;
    let Some(kind) = value.get("type") else {
        return Err(CredentialError::Malformed);
    };
    if serde_json::from_value::<super::model::CredentialType>(kind.clone()).is_err() {
        return if kind.is_string() || kind.is_number() {
            Ok(None)
        } else {
            Err(CredentialError::Malformed)
        };
    }
    let credential: Credential =
        serde_json::from_str(raw).map_err(|_| CredentialError::Malformed)?;
    credential.validate()?;
    Ok(Some(credential))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct NeverCancelled;
    impl Cancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }
    static NEVER: NeverCancelled = NeverCancelled;

    struct AlreadyCancelled;
    impl Cancellation for AlreadyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    static CANCELLED: AlreadyCancelled = AlreadyCancelled;

    struct TestCancellation {
        state: std::sync::atomic::AtomicBool,
        wake: tokio::sync::Notify,
        observed: tokio::sync::Notify,
    }
    impl TestCancellation {
        fn new() -> Self {
            Self {
                state: false.into(),
                wake: tokio::sync::Notify::new(),
                observed: tokio::sync::Notify::new(),
            }
        }
        fn cancel(&self) {
            self.state.store(true, Ordering::Release);
            self.wake.notify_waiters();
        }
    }
    impl Cancellation for TestCancellation {
        fn is_cancelled(&self) -> bool {
            self.state.load(Ordering::Acquire)
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                loop {
                    let notified = self.wake.notified();
                    if self.is_cancelled() {
                        self.observed.notify_one();
                        return;
                    }
                    notified.await;
                }
            })
        }
    }

    fn value<T>(result: OperationResult<T, CredentialError>) -> Result<T, CredentialError> {
        result.map_err(|error| match error {
            OperationError::Cancelled => panic!("unexpected cancellation"),
            OperationError::Failed(error) => error,
        })
    }

    fn path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "parrot-credentials-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ))
            .join("credentials.json")
    }

    #[tokio::test]
    async fn versioned_union_round_trips_and_sorted_list() {
        let path = path();
        let store = CredentialStore::at_selected_path(&path);
        value(
            store
                .set("z", &Credential::for_api_key("fake-key"), &NEVER)
                .await,
        )
        .unwrap();
        let serialized = fs::read(store.path()).unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&serialized).is_ok());
        assert!(value(store.list(&NEVER).await).is_ok());
        let s3 = Credential::for_s3("access", "secret");
        assert!(s3.validate().is_ok());
        let s3_json = serde_json::to_string(&s3).unwrap();
        assert!(indented_json(&s3_json).is_ok());
        value(store.set("a", &s3, &NEVER).await).unwrap();
        assert_eq!(value(store.list(&NEVER).await).unwrap(), ["a", "z"]);
        assert_eq!(
            value(store.get("z", &NEVER).await).unwrap().unwrap(),
            Credential::for_api_key("fake-key")
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn unknown_raw_entry_survives_unrelated_write() {
        let path = path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"version":1,"credentials":{"future":{"type":"new","n":1.2300,"nested":[true,{"raw":"x"}]}}}"#).unwrap();
        let store = CredentialStore::at_selected_path(&path);
        assert_eq!(
            value(store.get("future", &NEVER).await).unwrap_err(),
            CredentialError::Malformed
        );
        value(
            store
                .set("known", &Credential::for_api_key("fake"), &NEVER)
                .await,
        )
        .unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("1.2300"));
        assert!(content.contains("\"type\": \"new\""));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn admission_cancellation_does_not_wait_for_gate_release() {
        let path = path();
        let store = CredentialStore::at_selected_path(&path);
        let held = store.lock.clone().lock_owned().await;
        let cancellation = TestCancellation::new();
        let mut admission = Box::pin(admission(store.lock.clone(), &cancellation));
        tokio::select! {
            biased;
            result = &mut admission => panic!("gate unexpectedly acquired: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        cancellation.cancel();
        assert!(matches!(admission.await, Err(OperationError::Cancelled)));
        drop(held);
        assert!(value(store.list(&NEVER).await).unwrap().is_empty());
    }

    #[tokio::test]
    async fn canceled_blocking_stage_joins_owned_worker() {
        let mutex = Arc::new(tokio::sync::Mutex::new(()));
        let lease = Arc::new(mutex.lock_owned().await);
        let cancellation = Arc::new(TestCancellation::new());
        let (started_send, started_receive) = tokio::sync::oneshot::channel();
        let (release_send, release_receive) = std::sync::mpsc::channel();
        let worker_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            cancelable_blocking(lease, worker_cancellation.as_ref(), move || {
                let _ = started_send.send(());
                let _ = release_receive.recv();
                Ok(())
            })
            .await
        });
        started_receive.await.unwrap();
        cancellation.cancel();
        cancellation.observed.notified().await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "cancellation must settle the owned worker first"
        );
        release_send.send(()).unwrap();
        assert!(matches!(
            task.await.unwrap(),
            Err(OperationError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancellation_precedence_matches_store_boundaries() {
        let path = path();
        let store = CredentialStore::at_selected_path(&path);
        assert!(matches!(
            store.get("missing", &CANCELLED).await,
            Err(OperationError::Cancelled)
        ));
        assert!(matches!(
            store.list(&CANCELLED).await,
            Err(OperationError::Cancelled)
        ));
        assert!(matches!(
            store.delete("missing", &CANCELLED).await,
            Err(OperationError::Cancelled)
        ));
        assert!(matches!(
            store
                .set("", &Credential::for_api_key("k"), &CANCELLED)
                .await,
            Err(OperationError::Failed(CredentialError::Malformed))
        ));
        let mut invalid_credential = Credential::for_api_key("k");
        invalid_credential.api_key = None;
        assert!(matches!(
            store.set("valid", &invalid_credential, &CANCELLED).await,
            Err(OperationError::Failed(CredentialError::Invalid(
                super::super::model::CredentialType::ApiKey
            )))
        ));
        assert!(matches!(
            store
                .set("valid", &Credential::for_api_key("k"), &CANCELLED)
                .await,
            Err(OperationError::Cancelled)
        ));
        assert!(!path.exists());
    }

    fn phase_gate(
        phase: WritePhase,
    ) -> (
        WritePhaseHook,
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, held) = std::sync::mpsc::channel();
        let held = Mutex::new(held);
        let hook = Arc::new(move |current| {
            if current == phase {
                let signal = entered.lock().unwrap().take();
                if let Some(signal) = signal {
                    let _ = signal.send(());
                    let _ = held.lock().unwrap().recv();
                }
            }
        });
        (hook, waiting, release)
    }

    fn temporary_count(path: &Path) -> usize {
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(".credentials-") && name.ends_with(".tmp")
            })
            .count()
    }

    async fn cancellation_at_production_phase(phase: WritePhase) {
        let path = path();
        let mut store = CredentialStore::at_selected_path(&path);
        value(
            store
                .set("original", &Credential::for_api_key("old"), &NEVER)
                .await,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let (hook, entered, release) = phase_gate(phase);
        store.phase_hook = Some(hook);
        let store = Arc::new(store);
        let cancellation = Arc::new(TestCancellation::new());
        let operation_store = store.clone();
        let operation_cancellation = cancellation.clone();
        let operation = tokio::spawn(async move {
            operation_store
                .set(
                    "new",
                    &Credential::for_api_key("new"),
                    operation_cancellation.as_ref(),
                )
                .await
        });
        entered.await.unwrap();
        assert_eq!(temporary_count(&path), 1);
        cancellation.cancel();
        cancellation.observed.notified().await;
        assert!(!operation.is_finished());
        assert!(store.lock.try_lock().is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        release.send(()).unwrap();
        assert!(matches!(
            operation.await.unwrap(),
            Err(OperationError::Cancelled)
        ));
        assert_eq!(temporary_count(&path), 0);
        assert_eq!(fs::read(&path).unwrap(), before);
        value(
            store
                .set("later", &Credential::for_api_key("later"), &NEVER)
                .await,
        )
        .unwrap();
        assert_eq!(
            value(store.list(&NEVER).await).unwrap(),
            ["later", "original"]
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn production_write_cancellation_waits_for_worker_and_cleans_temporary() {
        cancellation_at_production_phase(WritePhase::Write).await;
    }

    #[tokio::test]
    async fn production_flush_cancellation_waits_for_worker_and_cleans_temporary() {
        cancellation_at_production_phase(WritePhase::Flush).await;
    }

    #[tokio::test]
    async fn production_commit_after_final_boundary_ignores_cancellation() {
        let path = path();
        let mut store = CredentialStore::at_selected_path(&path);
        value(
            store
                .set("original", &Credential::for_api_key("old"), &NEVER)
                .await,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let (hook, entered, release) = phase_gate(WritePhase::Commit);
        store.phase_hook = Some(hook);
        let store = Arc::new(store);
        let cancellation = Arc::new(TestCancellation::new());
        let operation_store = store.clone();
        let operation_cancellation = cancellation.clone();
        let operation = tokio::spawn(async move {
            operation_store
                .set(
                    "new",
                    &Credential::for_api_key("new"),
                    operation_cancellation.as_ref(),
                )
                .await
        });
        entered.await.unwrap();
        cancellation.cancel();
        assert!(!operation.is_finished());
        assert_eq!(fs::read(&path).unwrap(), before);
        release.send(()).unwrap();
        value(operation.await.unwrap()).unwrap();
        assert_eq!(temporary_count(&path), 0);
        assert_eq!(
            value(store.list(&NEVER).await).unwrap(),
            ["new", "original"]
        );
        assert_eq!(
            value(store.get("new", &NEVER).await).unwrap(),
            Some(Credential::for_api_key("new"))
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn dropped_production_commit_observer_retains_admission_until_settlement() {
        let path = path();
        let mut store = CredentialStore::at_selected_path(&path);
        value(
            store
                .set("original", &Credential::for_api_key("old"), &NEVER)
                .await,
        )
        .unwrap();
        let (hook, entered, release) = phase_gate(WritePhase::Commit);
        store.phase_hook = Some(hook);
        let store = Arc::new(store);
        let operation_store = store.clone();
        let operation = tokio::spawn(async move {
            operation_store
                .set("new", &Credential::for_api_key("new"), &NEVER)
                .await
        });
        entered.await.unwrap();
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
        assert!(store.lock.try_lock().is_err());
        let mut second = store.list(&NEVER);
        let first_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx))).await;
        assert!(first_poll.is_pending());
        release.send(()).unwrap();
        assert_eq!(value(second.await).unwrap(), ["new", "original"]);
        assert_eq!(temporary_count(&path), 0);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn file_private_and_size_limit() {
        let path = path();
        let store = CredentialStore::at_selected_path(&path);
        value(
            store
                .set("x", &Credential::for_api_key("fake"), &NEVER)
                .await,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                FILE_MODE
            );
        }
        fs::write(&path, vec![b' '; (MAX_STORE_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            value(store.list(&NEVER).await).unwrap_err(),
            CredentialError::StoreTooLarge
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
