//! Descriptor-relative port of CompactionGroupBlobStore.cs at 5f27a8a9.
//!
//! Source-shaped compaction messages are converted to the persisted artifact
//! envelope here, including file-backed image reads and base64 encoding.
//! No text, tool argument, image, or serialized artifact size limit is imposed.

use std::fmt;
use std::io;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::filenames::FilenameGenerator;
use parrot_domain::contracts::paths::SelectedSessionRoot;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde::Serialize;
use serde_json::Value;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::ser::{CharEscape, Formatter, PrettyFormatter};

#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAXIMUM_NAME_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionErrorKind {
    InvalidOperation,
    Io,
    UnsupportedPlatform,
    Cancelled,
}

/// Cleanup failures never replace the primary publication/cancellation failure.
#[derive(Debug)]
pub struct CompactionError {
    pub kind: CompactionErrorKind,
    pub message: String,
    pub native_error: Option<io::Error>,
    pub cleanup_failures: Vec<CompactionError>,
}

impl CompactionError {
    fn new(kind: CompactionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            native_error: None,
            cleanup_failures: Vec::new(),
        }
    }

    fn io(operation: &str, error: impl Into<io::Error>) -> Self {
        let error = error.into();
        let mut display = error.to_string();
        if let Some(code) = error.raw_os_error() {
            let suffix = format!(" (os error {code})");
            if let Some(source_message) = display.strip_suffix(&suffix) {
                display = source_message.to_owned();
            }
        }
        Self {
            kind: CompactionErrorKind::Io,
            message: format!("Failed to {operation}: {display}"),
            native_error: Some(error),
            cleanup_failures: Vec::new(),
        }
    }

    fn invalid(message: &str) -> Self {
        Self::new(CompactionErrorKind::InvalidOperation, message)
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.native_error
            .as_ref()
            .map(|error| error as &dyn std::error::Error)
    }
}

fn check(cancellation: &dyn Cancellation) -> Result<(), CompactionError> {
    cancellation.check().map_err(|_| {
        CompactionError::new(
            CompactionErrorKind::Cancelled,
            "The operation was canceled.",
        )
    })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
type CleanupSelectionHook = Box<dyn Fn(&str)>;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[derive(Default)]
pub(crate) struct Hooks {
    pub after_blob_directory_opened: Option<Box<dyn Fn()>>,
    pub after_file_created: Option<Box<dyn Fn()>>,
    pub after_final_link_created: Option<Box<dyn Fn()>>,
    pub after_cleanup_selection: Option<CleanupSelectionHook>,
    #[cfg(target_os = "linux")]
    pub returned_status_mask: Option<Box<dyn Fn(u32) -> u32>>,
}

/// The ordered messages to persist for a source compaction group.
pub struct CompactionGroup {
    pub messages: Vec<CompactionMessage>,
}

/// Source message fields retained by the compaction artifact.
pub struct CompactionMessage {
    pub role: String,
    pub contents: Vec<CompactionContent>,
    pub tool_calls: Vec<CompactionToolCall>,
    pub tool_call_id: String,
}

/// A text or file-backed image content item.
pub struct CompactionContent {
    pub kind: String,
    pub text: String,
    pub image_path: Option<PathBuf>,
    pub media_type: String,
}

/// A tool call with its original arguments JSON text.
pub struct CompactionToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

impl CompactionGroup {
    fn to_artifact(&self) -> Result<Value, CompactionError> {
        let mut messages = Vec::with_capacity(self.messages.len());
        for message in &self.messages {
            let mut contents = Vec::with_capacity(message.contents.len());
            for content in &message.contents {
                let image = if content.kind == "Image" {
                    let path = content.image_path.as_ref().ok_or_else(|| {
                        CompactionError::invalid(
                            "An image compaction content item has no file path.",
                        )
                    })?;
                    let bytes = std::fs::read(path).map_err(|error| {
                        CompactionError::io("read compaction group image", error)
                    })?;
                    encode_base64(&bytes)
                } else {
                    String::new()
                };
                contents.push(serde_json::json!({
                    "kind": content.kind,
                    "text": content.text,
                    "image": image,
                    "media_type": content.media_type,
                }));
            }
            let tool_calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    serde_json::json!({
                        "id": call.id,
                        "name": call.name,
                        "arguments_json": call.arguments_json,
                    })
                })
                .collect::<Vec<_>>();
            messages.push(serde_json::json!({
                "role": message.role,
                "contents": contents,
                "tool_calls": tool_calls,
                "tool_call_id": message.tool_call_id,
            }));
        }
        Ok(serde_json::json!({ "messages": messages }))
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            encoded.push(char::from(
                ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

pub struct CompactionGroupBlobStore {
    blob_directory: PathBuf,
    agent_directory: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    filename_generator: Arc<dyn FilenameGenerator>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    blob_identity: Option<native::Identity>,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) hooks: Hooks,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) force_named_file: bool,
    #[cfg(test)]
    pub(crate) unsupported_platform: bool,
}

impl CompactionGroupBlobStore {
    /// Binds the selected session's agent resource to its existing `blobs`
    /// directory. Construction is lazy: it neither opens nor creates files.
    ///
    /// The generator supplies a complete basename. Only its terminal
    /// `-arse.dat` suffix becomes `.json`.
    pub fn for_session(
        root: &SelectedSessionRoot,
        relative_agent_resource: impl AsRef<Path>,
        filename_generator: Arc<dyn FilenameGenerator>,
    ) -> Result<Self, CompactionError> {
        let agent = root
            .resolve_internal(relative_agent_resource)
            .map_err(|_| {
                CompactionError::invalid(
                    "The compaction group blob directory escaped the agent scratch directory.",
                )
            })?;
        let agent_directory = agent.path().to_path_buf();
        let store = Self {
            blob_directory: agent_directory.join("blobs"),
            agent_directory,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            filename_generator,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            blob_identity: None,
            #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
            hooks: Hooks::default(),
            #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
            force_named_file: false,
            #[cfg(test)]
            unsupported_platform: false,
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = filename_generator;
        store.validate_blob_directory()?;
        Ok(store)
    }

    fn validate_blob_directory(&self) -> Result<(), CompactionError> {
        if !self.blob_directory.is_absolute()
            || self.blob_directory != self.agent_directory.join("blobs")
            || !self.blob_directory.starts_with(&self.agent_directory)
        {
            return Err(CompactionError::invalid(
                "The compaction group blob directory escaped the agent scratch directory.",
            ));
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn next_name(&self) -> Result<String, CompactionError> {
        let generated = self.filename_generator.next_filename();
        let name = generated
            .strip_suffix("-arse.dat")
            .map_or_else(|| generated.clone(), |stem| format!("{stem}.json"));
        if name.contains(['/', '\\'])
            || Path::new(&name)
                .file_name()
                .and_then(|value| value.to_str())
                != Some(name.as_str())
            || !name.ends_with(".json")
            || name.len() == ".json".len()
        {
            return Err(CompactionError::invalid(
                "The compaction group blob name must be a safe .json basename.",
            ));
        }
        Ok(name)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn confine(&self, name: &str) -> Result<PathBuf, CompactionError> {
        let path = self.blob_directory.join(name);
        if !path.starts_with(&self.agent_directory)
            || path.parent() != Some(self.blob_directory.as_path())
        {
            return Err(CompactionError::invalid(
                "A compaction group blob escaped the agent scratch blob directory.",
            ));
        }
        Ok(path)
    }

    pub fn persist(
        &mut self,
        group: &CompactionGroup,
        cancellation: &dyn Cancellation,
    ) -> Result<PathBuf, CompactionError> {
        self.validate_blob_directory()?;
        check(cancellation)?;
        #[cfg(test)]
        if self.unsupported_platform {
            return Err(CompactionError::new(
                CompactionErrorKind::UnsupportedPlatform,
                "Race-safe compaction group persistence is unavailable on this platform.",
            ));
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let artifact = group.to_artifact()?;
            let directory = native::open_directory(&self.blob_directory)?;
            #[cfg(test)]
            if let Some(hook) = &self.hooks.after_blob_directory_opened {
                hook();
            }
            self.require_current(&directory)?;
            #[cfg(target_os = "linux")]
            {
                #[cfg(test)]
                let force_named = self.force_named_file;
                #[cfg(not(test))]
                let force_named = false;
                if !force_named {
                    if let Some(file) = native::try_open_temporary_file(&directory)? {
                        return self.persist_file(&directory, file, None, &artifact, cancellation);
                    }
                }
            }
            let (staging_name, file) = loop {
                let name = native::unique_name("tmp")?;
                if let Some(file) = native::try_open_named_file(&directory, &name)? {
                    break (name, file);
                }
            };
            self.persist_file(
                &directory,
                file,
                Some(staging_name),
                &artifact,
                cancellation,
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = group;
            Err(CompactionError::new(
                CompactionErrorKind::UnsupportedPlatform,
                "Race-safe compaction group persistence is unavailable on this platform.",
            ))
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn require_current(&mut self, directory: &std::fs::File) -> Result<(), CompactionError> {
        let opened = native::identify(directory, true, self)?;
        match self.blob_identity {
            None => self.blob_identity = Some(opened),
            Some(expected) if expected != opened => {
                return Err(CompactionError::invalid(
                    "The compaction group blob directory is not the invoking agent's current blob directory.",
                ));
            }
            _ => {}
        }
        let current = native::open_directory(&self.blob_directory)?;
        if Some(native::identify(&current, true, self)?) != self.blob_identity {
            return Err(CompactionError::invalid(
                "The compaction group blob directory changed while publishing an artifact.",
            ));
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn persist_file(
        &mut self,
        directory: &std::fs::File,
        mut file: std::fs::File,
        staging_name: Option<String>,
        artifact: &Value,
        cancellation: &dyn Cancellation,
    ) -> Result<PathBuf, CompactionError> {
        let mut identity = None;
        let mut published_name = None;
        let result = (|| {
            if staging_name.is_some() {
                identity = Some(native::identify(&file, false, self)?);
            }
            #[cfg(test)]
            if let Some(hook) = &self.hooks.after_file_created {
                hook();
            }
            serialize(&mut file, artifact, cancellation)?;
            self.require_current(directory)?;
            if identity.is_none() {
                identity = Some(native::identify(&file, false, self)?);
            }
            for _ in 0..MAXIMUM_NAME_ATTEMPTS {
                check(cancellation)?;
                let name = self.next_name()?;
                let path = self.confine(&name)?;
                if !native::publish(&file, directory, staging_name.as_deref(), &name)? {
                    continue;
                }
                published_name = Some(name);
                #[cfg(test)]
                if let Some(hook) = &self.hooks.after_final_link_created {
                    hook();
                }
                self.require_current(directory)?;
                if let Some(staging) = &staging_name {
                    native::delete_if_same(
                        directory,
                        staging,
                        identity.expect("identified before publish"),
                        self,
                    )?;
                }
                return Ok(path);
            }
            Err(CompactionError::new(
                CompactionErrorKind::Io,
                format!(
                    "Could not create a unique compaction group blob in '{}'.",
                    self.blob_directory.display()
                ),
            ))
        })();
        // Keep both descriptors alive throughout cleanup, even after relocation.
        if let Err(mut failure) = result {
            if let Some(identity) = identity {
                for name in [published_name.as_deref(), staging_name.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    if let Err(cleanup) = native::delete_if_same(directory, name, identity, self) {
                        failure.cleanup_failures.push(cleanup);
                    }
                }
            }
            return Err(failure);
        }
        result
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct CancellableWriter<'a> {
    file: io::BufWriter<&'a mut std::fs::File>,
    cancellation: &'a dyn Cancellation,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Write for CancellableWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("A task was canceled."));
        }
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("A task was canceled."));
        }
        self.file.flush()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SourceJsonFormatter(PrettyFormatter<'static>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Formatter for SourceJsonFormatter {
    fn write_char_escape<W>(&mut self, writer: &mut W, escape: CharEscape) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        match escape {
            CharEscape::Quote => writer.write_all(b"\\u0022"),
            escape => self.0.write_char_escape(writer, escape),
        }
    }

    fn begin_array<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.begin_array(writer)
    }

    fn end_array<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.end_array(writer)
    }

    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.end_array_value(writer)
    }

    fn begin_object<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.begin_object(writer)
    }

    fn end_object<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.end_object(writer)
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.begin_object_key(writer, first)
    }

    fn end_object_key<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.end_object_key(writer)
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        self.0.end_object_value(writer)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn serialize(
    file: &mut std::fs::File,
    artifact: &Value,
    cancellation: &dyn Cancellation,
) -> Result<(), CompactionError> {
    let mut writer = CancellableWriter {
        file: io::BufWriter::new(file),
        cancellation,
    };
    let serialized = {
        let formatter = SourceJsonFormatter(PrettyFormatter::new());
        let mut serializer = serde_json::Serializer::with_formatter(&mut writer, formatter);
        artifact.serialize(&mut serializer)
    };
    if let Err(error) = serialized {
        if cancellation.is_cancelled() {
            return Err(CompactionError::new(
                CompactionErrorKind::Cancelled,
                "A task was canceled.",
            ));
        }
        return Err(CompactionError::io(
            "serialize a compaction group artifact",
            io::Error::other(error),
        ));
    }
    writer.flush().map_err(|error| {
        if cancellation.is_cancelled() {
            CompactionError::new(CompactionErrorKind::Cancelled, "A task was canceled.")
        } else {
            CompactionError::io("flush a compaction group artifact", error)
        }
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use super::*;
    #[cfg(target_os = "linux")]
    use rustix::fd::AsFd;
    use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags};
    use rustix::io::Errno;
    use std::fs::File;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) struct Identity {
        device: u64,
        inode: u64,
    }

    pub(super) fn open_directory(path: &Path) -> Result<File, CompactionError> {
        if !path.is_absolute() {
            return Err(CompactionError::invalid(
                "The compaction group blob directory must be absolute.",
            ));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let open_failure = |error| {
            let mut error = CompactionError::io("open the compaction group blob directory", error);
            error.kind = CompactionErrorKind::InvalidOperation;
            error
        };
        let mut directory = fs::open("/", flags, Mode::empty()).map_err(open_failure)?;
        for component in path.components() {
            if let std::path::Component::Normal(component) = component {
                directory = fs::openat(&directory, component, flags, Mode::empty())
                    .map_err(open_failure)?;
            }
        }
        Ok(directory.into())
    }

    fn open_stream(fd: rustix::fd::OwnedFd) -> Result<File, CompactionError> {
        fs::fchmod(&fd, Mode::RUSR | Mode::WUSR)
            .map_err(|error| CompactionError::io("secure a compaction group artifact", error))?;
        Ok(fd.into())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn try_open_temporary_file(
        directory: &File,
    ) -> Result<Option<File>, CompactionError> {
        match fs::openat(
            directory,
            ".",
            OFlags::WRONLY | OFlags::TMPFILE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => open_stream(fd).map(Some),
            Err(Errno::NOENT | Errno::ISDIR | Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
                Ok(None)
            }
            Err(error) => Err(CompactionError::io(
                "create an unnamed compaction group staging file",
                error,
            )),
        }
    }

    pub(super) fn try_open_named_file(
        directory: &File,
        name: &str,
    ) -> Result<Option<File>, CompactionError> {
        match fs::openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => open_stream(fd).map(Some),
            Err(Errno::EXIST) => Ok(None),
            Err(error) => Err(CompactionError::io(
                "create a compaction group artifact",
                error,
            )),
        }
    }

    pub(super) fn unique_name(suffix: &str) -> Result<String, CompactionError> {
        let id = parrot_domain::values::identifiers::Identifier::event_id().map_err(|error| {
            CompactionError::io("reserve a compaction group temporary name", error)
        })?;
        Ok(format!(".{}.{}", id.replace('-', ""), suffix))
    }

    #[cfg(target_os = "linux")]
    fn status(
        directory: impl AsFd,
        name: &str,
        flags: AtFlags,
        store: &CompactionGroupBlobStore,
    ) -> Result<(Identity, u16), CompactionError> {
        let required = fs::StatxFlags::TYPE | fs::StatxFlags::INO;
        let status = fs::statx(directory, name, flags, required)
            .map_err(|error| CompactionError::io("inspect a compaction group artifact", error))?;
        let mask = status.stx_mask;
        #[cfg(test)]
        let mask = store
            .hooks
            .returned_status_mask
            .as_ref()
            .map_or(mask, |hook| hook(mask));
        #[cfg(not(test))]
        let _ = store;
        if mask & required.bits() != required.bits() {
            return Err(CompactionError::new(
                CompactionErrorKind::Io,
                "The operating system did not return all required compaction group identity fields.",
            ));
        }
        Ok((
            Identity {
                device: (u64::from(status.stx_dev_major) << 32) | u64::from(status.stx_dev_minor),
                inode: status.stx_ino,
            },
            status.stx_mode,
        ))
    }

    pub(super) fn identify(
        file: &File,
        directory: bool,
        store: &CompactionGroupBlobStore,
    ) -> Result<Identity, CompactionError> {
        #[cfg(target_os = "linux")]
        let (identity, mode) = status(file, "", AtFlags::EMPTY_PATH, store)?;
        #[cfg(target_os = "macos")]
        let (identity, mode) = {
            let _ = store;
            let status = fs::fstat(file).map_err(|error| {
                CompactionError::io("inspect a compaction group artifact", error)
            })?;
            (
                Identity {
                    device: u64::from(status.st_dev as u32),
                    inode: status.st_ino,
                },
                status.st_mode,
            )
        };
        if directory && mode & 0xf000 != 0x4000 {
            return Err(CompactionError::invalid(
                "The compaction group blob path is not a directory.",
            ));
        }
        Ok(identity)
    }

    fn identify_entry(
        directory: &File,
        name: &str,
        store: &CompactionGroupBlobStore,
    ) -> Result<Option<Identity>, CompactionError> {
        #[cfg(target_os = "linux")]
        let result =
            status(directory, name, AtFlags::SYMLINK_NOFOLLOW, store).map(|(identity, _)| identity);
        #[cfg(target_os = "macos")]
        let result = {
            let _ = store;
            fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|status| Identity {
                    device: u64::from(status.st_dev as u32),
                    inode: status.st_ino,
                })
                .map_err(|error| CompactionError::io("inspect a compaction group artifact", error))
        };
        match result {
            Ok(identity) => Ok(Some(identity)),
            Err(error)
                if error
                    .native_error
                    .as_ref()
                    .and_then(io::Error::raw_os_error)
                    == Some(Errno::NOENT.raw_os_error()) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn publish(
        file: &File,
        directory: &File,
        staging: Option<&str>,
        name: &str,
    ) -> Result<bool, CompactionError> {
        let result = match staging {
            Some(staging) => fs::linkat(directory, staging, directory, name, AtFlags::empty()),
            #[cfg(target_os = "linux")]
            None => fs::linkat(file, "", directory, name, AtFlags::EMPTY_PATH),
            #[cfg(target_os = "macos")]
            None => {
                let _ = file;
                unreachable!("Darwin always uses a named staging file")
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(Errno::EXIST) => Ok(false),
            Err(error) => Err(CompactionError::io(
                "publish a compaction group artifact",
                error,
            )),
        }
    }

    fn exchange(directory: &File, from: &str, to: &str) -> rustix::io::Result<()> {
        // Linux renameat2(RENAME_EXCHANGE); Darwin renameatx_np(RENAME_SWAP).
        fs::renameat_with(directory, from, directory, to, RenameFlags::EXCHANGE)
    }

    fn remove_placeholder(directory: &File, name: &str) -> Result<(), CompactionError> {
        fs::unlinkat(directory, name, AtFlags::REMOVEDIR).map_err(|error| {
            CompactionError::io("remove a compaction group cleanup placeholder", error)
        })
    }

    pub(super) fn delete_if_same(
        directory: &File,
        name: &str,
        expected: Identity,
        store: &CompactionGroupBlobStore,
    ) -> Result<(), CompactionError> {
        for _ in 0..MAXIMUM_NAME_ATTEMPTS {
            let quarantine = unique_name("cleanup")?;
            match fs::mkdirat(directory, quarantine.as_str(), Mode::RWXU) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(error) => {
                    return Err(CompactionError::io(
                        "reserve a compaction group cleanup placeholder",
                        error,
                    ));
                }
            }
            if let Err(error) = exchange(directory, name, &quarantine) {
                remove_placeholder(directory, &quarantine)?;
                if error == Errno::NOENT {
                    return Ok(());
                }
                return Err(CompactionError::io(
                    "quarantine a compaction group artifact",
                    error,
                ));
            }
            // The old name is now a DIRECTORY, not a free filename. Inspect and
            // unlink only the atomically selected entry, never stat-then-unlink
            // the publication name. Unrelated entries are swapped back.
            let selection = (|| {
                #[cfg(test)]
                if let Some(hook) = &store.hooks.after_cleanup_selection {
                    hook(name);
                }
                if identify_entry(directory, &quarantine, store)? == Some(expected) {
                    fs::unlinkat(directory, quarantine.as_str(), AtFlags::empty()).map_err(
                        |error| {
                            CompactionError::io(
                                "remove a quarantined compaction group artifact",
                                error,
                            )
                        },
                    )?;
                    remove_placeholder(directory, name)?;
                    return Ok(true);
                }
                Ok(false)
            })();
            if matches!(selection, Ok(true)) {
                return Ok(());
            }
            exchange(directory, &quarantine, name).map_err(|error| {
                CompactionError::io("restore an unrelated quarantined entry", error)
            })?;
            remove_placeholder(directory, &quarantine)?;
            return selection.map(|_| ());
        }
        Err(CompactionError::new(
            CompactionErrorKind::Io,
            "Could not reserve a unique quarantine name for compaction group cleanup.",
        ))
    }
}
