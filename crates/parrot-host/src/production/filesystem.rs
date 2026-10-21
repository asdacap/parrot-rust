use super::policy::ProductionPolicy;
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
        },
    },
    values::project_workspace::ProjectWorkspace,
};
use parrot_runtime::tool_settlement::SharedSecurity;
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
pub struct ProductionFilesystem {
    root: PathBuf,
}

type Security = SharedSecurity<ProductionPolicy>;
type Result<T> = OperationResult<T, FileReadError>;

fn failed(kind: FileReadErrorKind, message: impl Into<String>) -> OperationError<FileReadError> {
    OperationError::Failed(FileReadError::new(kind, message))
}

fn normalize(root: &Path, requested: &Path) -> Result<PathBuf> {
    let input = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let mut result = PathBuf::new();
    for part in input.components() {
        match part {
            Component::RootDir => result.push(Path::new("/")),
            Component::Normal(value) => result.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Prefix(_) => {
                return Err(failed(
                    FileReadErrorKind::InvalidPath,
                    "Unsupported path prefix.",
                ));
            }
        }
    }
    if !result.is_absolute() || result.as_os_str().is_empty() {
        return Err(failed(
            FileReadErrorKind::InvalidPath,
            "A filesystem path must be absolute.",
        ));
    }
    Ok(result)
}

fn check_cancel(cancellation: &dyn Cancellation) -> Result<()> {
    cancellation.check().map_err(|_| OperationError::Cancelled)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported<T>() -> Result<T> {
    Err(failed(
        FileReadErrorKind::NotSupported,
        "Descriptor-bound production filesystem operations are unavailable on this platform.",
    ))
}

impl ProductionFilesystem {
    pub fn new(workspace: ProjectWorkspace) -> std::result::Result<Self, FileReadError> {
        if !workspace.launch_directory().is_absolute() {
            return Err(FileReadError::new(
                FileReadErrorKind::InvalidPath,
                "The workspace launch directory must be absolute.",
            ));
        }
        let root = normalize(Path::new("/"), workspace.launch_directory()).map_err(|error| {
            FileReadError::new(FileReadErrorKind::InvalidPath, error.to_string())
        })?;
        let physical = std::fs::canonicalize(&root).map_err(|error| {
            FileReadError::new(FileReadErrorKind::InvalidPath, error.to_string())
        })?;
        if !physical.is_dir() {
            return Err(FileReadError::new(
                FileReadErrorKind::InvalidPath,
                "The workspace is not a directory.",
            ));
        }
        Ok(Self { root })
    }

    fn path(&self, requested: &Path) -> Result<PathBuf> {
        normalize(&self.root, requested)
    }

    fn target(&self, path: PathBuf) -> MutationTarget {
        let root = self.root.components().collect::<Vec<_>>();
        let target = path.components().collect::<Vec<_>>();
        let common = root
            .iter()
            .zip(&target)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative = PathBuf::new();
        for _ in common..root.len() {
            relative.push("..");
        }
        for part in &target[common..] {
            relative.push(part.as_os_str());
        }
        if relative.as_os_str().is_empty() {
            relative.push(".");
        }
        MutationTarget {
            physical_path: path,
            display_path: relative.to_string_lossy().into_owned(),
        }
    }
}

impl AuthorizedReadService for ProductionFilesystem {
    type Security = Security;

    fn inspect<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<FileMetadata>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                unix_fs::inspect(&self.path(path)?, links, snapshot)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (path, links, snapshot);
                unsupported()
            }
        })
    }

    fn list_directory<'a>(
        &'a self,
        path: &'a Path,
        links: LinkPolicy,
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<FileMetadata>>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                unix_fs::list_directory(&self.path(path)?, links, snapshot, cancellation)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (path, links, snapshot);
                unsupported()
            }
        })
    }

    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<BoundRead>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                unix_fs::read(
                    &self.path(&request.requested)?,
                    request,
                    snapshot,
                    cancellation,
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (request, snapshot);
                unsupported()
            }
        })
    }
}

impl AuthorizedMutationService for ProductionFilesystem {
    type Security = Security;

    fn inspect_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<MutationTarget>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let path = self.path(&request.requested)?;
                unix_fs::inspect_mutation(&path, request.create, snapshot, cancellation)?;
                Ok(self.target(path))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (request, snapshot);
                unsupported()
            }
        })
    }

    fn read_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<MutationRead>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let path = self.path(&request.requested)?;
                let pinned =
                    unix_fs::inspect_mutation(&path, request.create, snapshot, cancellation)?;
                let bytes = pinned
                    .map(|pinned| unix_fs::read_mutation(&path, pinned, snapshot, cancellation))
                    .transpose()?;
                Ok(MutationRead {
                    target: self.target(path),
                    bytes,
                })
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (request, snapshot);
                unsupported()
            }
        })
    }

    fn write_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        bytes: &'a [u8],
        snapshot: &'a Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<MutationTarget>> {
        Box::pin(async move {
            check_cancel(cancellation)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let path = self.path(&request.requested)?;
                unix_fs::write(&path, request.create, bytes, snapshot, cancellation)?;
                Ok(self.target(path))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (request, bytes, snapshot);
                unsupported()
            }
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_fs {
    use super::*;
    use parrot_domain::contracts::{
        filesystem::EntryKind,
        paths::{ReadOrigin, ResolvedRead},
        security::AccessSnapshot,
    };
    use std::{
        ffi::{CString, OsStr},
        fs::{self, File},
        io::{Read, Write},
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
            unix::ffi::OsStrExt,
        },
    };

    #[cfg(target_os = "linux")]
    const PIN_FLAGS: i32 = libc::O_PATH;
    #[cfg(target_os = "macos")]
    const PIN_FLAGS: i32 = libc::O_EVTONLY | libc::O_NONBLOCK;
    #[cfg(target_os = "linux")]
    const DIRECTORY_FLAGS: i32 = libc::O_PATH | libc::O_DIRECTORY;
    #[cfg(target_os = "macos")]
    const DIRECTORY_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK;

    #[cfg(target_os = "linux")]
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn c_path(path: &OsStr) -> Result<CString> {
        CString::new(path.as_bytes()).map_err(|_| {
            failed(
                FileReadErrorKind::InvalidPath,
                "A filesystem path contains a null byte.",
            )
        })
    }

    fn io(error: std::io::Error) -> OperationError<FileReadError> {
        let kind = match error.raw_os_error() {
            Some(libc::ENAMETOOLONG) => FileReadErrorKind::PathTooLong,
            Some(libc::ELOOP) => FileReadErrorKind::InvalidOperation,
            Some(libc::ENOTDIR) => FileReadErrorKind::InvalidOperation,
            Some(libc::ENOSYS | libc::EOPNOTSUPP) => FileReadErrorKind::NotSupported,
            _ => match error.kind() {
                std::io::ErrorKind::NotFound => FileReadErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied => FileReadErrorKind::Denied,
                std::io::ErrorKind::InvalidInput => FileReadErrorKind::InvalidPath,
                _ => FileReadErrorKind::Io,
            },
        };
        failed(kind, error.to_string())
    }

    fn changed() -> OperationError<FileReadError> {
        failed(
            FileReadErrorKind::TargetChanged,
            "The filesystem target changed during the operation.",
        )
    }

    #[cfg(target_os = "linux")]
    fn open_at(
        directory: RawFd,
        name: &OsStr,
        flags: i32,
        mode: u32,
        reject_links: bool,
    ) -> Result<OwnedFd> {
        let name = c_path(name)?;
        let specification = OpenHow {
            flags: u64::try_from(flags | libc::O_CLOEXEC).map_err(|_| {
                failed(
                    FileReadErrorKind::InvalidOperation,
                    "Invalid filesystem open flags.",
                )
            })?,
            mode: u64::from(mode),
            resolve: if reject_links { RESOLVE_NO_SYMLINKS } else { 0 },
        };
        // openat2 resolves all components and transfers the returned descriptor to OwnedFd.
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory,
                name.as_ptr(),
                &specification,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if descriptor < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        let descriptor = i32::try_from(descriptor)
            .map_err(|_| failed(FileReadErrorKind::Io, "Invalid filesystem descriptor."))?;
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    #[cfg(target_os = "macos")]
    fn open_component(directory: RawFd, name: &OsStr, flags: i32, mode: u32) -> Result<OwnedFd> {
        let name = c_path(name)?;
        // The C string is live for the call. Every opened component rejects links;
        // successful openat transfers a new descriptor into OwnedFd.
        let descriptor = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::mode_t,
            )
        };
        if descriptor < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    #[cfg(target_os = "macos")]
    fn open_at(
        directory: RawFd,
        name: &OsStr,
        flags: i32,
        mode: u32,
        _reject_links: bool,
    ) -> Result<OwnedFd> {
        let path = Path::new(name);
        if !path.is_absolute() {
            // All relative callers name a single child of an anchored directory.
            if path.components().count() != 1
                || !matches!(path.components().next(), Some(Component::Normal(_)))
            {
                return Err(failed(
                    FileReadErrorKind::InvalidPath,
                    "Expected one path component.",
                ));
            }
            return open_component(directory, name, flags, mode);
        }
        let names = path
            .components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut parent = open_component(libc::AT_FDCWD, OsStr::new("/"), DIRECTORY_FLAGS, 0)?;
        let Some((last, parents)) = names.split_last() else {
            return Ok(parent);
        };
        for name in parents {
            parent = open_component(parent.as_raw_fd(), name, DIRECTORY_FLAGS, 0)?;
        }
        open_component(parent.as_raw_fd(), last, flags, mode)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn pin(path: &Path, links: LinkPolicy) -> Result<OwnedFd> {
        open_at(
            libc::AT_FDCWD,
            path.as_os_str(),
            libc::O_PATH
                | if links == LinkPolicy::NoFollow {
                    libc::O_NOFOLLOW
                } else {
                    0
                },
            0,
            links == LinkPolicy::RejectLinks,
        )
    }

    #[cfg(target_os = "macos")]
    fn resolved_path(path: &Path, links: LinkPolicy) -> Result<PathBuf> {
        match links {
            LinkPolicy::RejectLinks => Ok(path.to_path_buf()),
            LinkPolicy::Follow => fs::canonicalize(path).map_err(io),
            LinkPolicy::NoFollow => match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => Ok(fs::canonicalize(parent).map_err(io)?.join(name)),
                _ => fs::canonicalize(path).map_err(io),
            },
        }
    }

    #[cfg(target_os = "macos")]
    fn validate_parent(descriptor: RawFd, path: &Path) -> Result<()> {
        if descriptor_path(descriptor)? != path {
            return Err(changed());
        }
        let current = open_at(libc::AT_FDCWD, path.as_os_str(), DIRECTORY_FLAGS, 0, true)
            .map_err(|_| changed())?;
        if !same(&stat(descriptor)?, &stat(current.as_raw_fd())?)
            || descriptor_path(current.as_raw_fd())? != path
            || descriptor_path(descriptor)? != path
        {
            return Err(changed());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn current_entry(path: &Path, links: LinkPolicy) -> Result<(PathBuf, libc::stat)> {
        let physical = resolved_path(path, links)?;
        let Some(name) = physical.file_name() else {
            let root = open_at(
                libc::AT_FDCWD,
                physical.as_os_str(),
                DIRECTORY_FLAGS,
                0,
                true,
            )?;
            if descriptor_path(root.as_raw_fd())? != physical {
                return Err(changed());
            }
            return Ok((physical, stat(root.as_raw_fd())?));
        };
        let parent_path = physical.parent().ok_or_else(changed)?;
        let parent = open_at(
            libc::AT_FDCWD,
            parent_path.as_os_str(),
            DIRECTORY_FLAGS,
            0,
            true,
        )?;
        validate_parent(parent.as_raw_fd(), parent_path)?;
        // Observe the final entry without opening or following it, including symlinks.
        let status = stat_at(parent.as_raw_fd(), name, true)?;
        validate_parent(parent.as_raw_fd(), parent_path)?;
        Ok((physical, status))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn pin(path: &Path, links: LinkPolicy) -> Result<OwnedFd> {
        let (physical, observed) = current_entry(path, links)?;
        let flags = if kind(&observed) == EntryKind::Directory {
            DIRECTORY_FLAGS
        } else {
            // O_EVTONLY pins metadata without requiring file read permission. Data
            // access is opened separately with O_NONBLOCK and checked against this inode.
            PIN_FLAGS
        };
        let descriptor = open_at(libc::AT_FDCWD, physical.as_os_str(), flags, 0, true)?;
        if !same(&observed, &stat(descriptor.as_raw_fd())?) {
            return Err(changed());
        }
        Ok(descriptor)
    }

    fn stat(descriptor: RawFd) -> Result<libc::stat> {
        let mut value = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, value.as_mut_ptr()) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { value.assume_init() })
    }

    fn stat_at(directory: RawFd, name: &OsStr, no_follow: bool) -> Result<libc::stat> {
        let name = c_path(name)?;
        let mut value = MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                value.as_mut_ptr(),
                if no_follow {
                    libc::AT_SYMLINK_NOFOLLOW
                } else {
                    0
                },
            )
        } != 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { value.assume_init() })
    }

    fn same(left: &libc::stat, right: &libc::stat) -> bool {
        left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_mode & libc::S_IFMT == right.st_mode & libc::S_IFMT
    }

    fn kind(status: &libc::stat) -> EntryKind {
        match status.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::Regular,
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFLNK => EntryKind::SymbolicLink,
            _ => EntryKind::Other,
        }
    }

    #[cfg(target_os = "linux")]
    fn descriptor_path(descriptor: RawFd) -> Result<PathBuf> {
        fs::read_link(format!("/proc/self/fd/{descriptor}")).map_err(io)
    }

    #[cfg(target_os = "macos")]
    fn descriptor_path(descriptor: RawFd) -> Result<PathBuf> {
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        // F_GETPATH writes at most PATH_MAX bytes into this live writable buffer.
        if unsafe { libc::fcntl(descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(changed)?;
        Ok(PathBuf::from(OsStr::from_bytes(&buffer[..end])))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn validate(
        descriptor: RawFd,
        lexical: &Path,
        links: LinkPolicy,
        expected: Option<&Path>,
    ) -> Result<(PathBuf, libc::stat)> {
        let status = stat(descriptor)?;
        let physical = descriptor_path(descriptor)?;
        if !physical.is_absolute()
            || (links == LinkPolicy::RejectLinks && physical != lexical)
            || expected.is_some_and(|expected| expected != physical)
        {
            return Err(changed());
        }
        let actual = stat_at(
            libc::AT_FDCWD,
            physical.as_os_str(),
            links == LinkPolicy::NoFollow,
        )
        .map_err(|_| changed())?;
        if !same(&status, &actual) {
            return Err(changed());
        }
        let current = if links == LinkPolicy::RejectLinks {
            let current = pin(lexical, LinkPolicy::RejectLinks).map_err(|_| changed())?;
            stat(current.as_raw_fd())?
        } else {
            stat_at(
                libc::AT_FDCWD,
                lexical.as_os_str(),
                links == LinkPolicy::NoFollow,
            )
            .map_err(|_| changed())?
        };
        if !same(&status, &current) {
            return Err(changed());
        }
        Ok((physical, status))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn validate(
        descriptor: RawFd,
        lexical: &Path,
        links: LinkPolicy,
        expected: Option<&Path>,
    ) -> Result<(PathBuf, libc::stat)> {
        let status = stat(descriptor)?;
        let physical = descriptor_path(descriptor)?;
        if !physical.is_absolute()
            || (links == LinkPolicy::RejectLinks && physical != lexical)
            || expected.is_some_and(|expected| expected != physical)
        {
            return Err(changed());
        }
        let (_, actual) =
            current_entry(&physical, LinkPolicy::RejectLinks).map_err(|_| changed())?;
        let (current_physical, current) = current_entry(lexical, links).map_err(|_| changed())?;
        if current_physical != physical
            || !same(&status, &actual)
            || !same(&status, &current)
            || descriptor_path(descriptor)? != physical
        {
            return Err(changed());
        }
        Ok((physical, status))
    }

    fn authorize_read(snapshot: &Security, lexical: &Path, physical: &Path) -> Result<()> {
        snapshot
            .check_read(&ResolvedRead::new(
                lexical.to_path_buf(),
                physical.to_path_buf(),
                ReadOrigin::Explicit,
            ))
            .map_err(OperationError::Failed)
    }

    fn authorize_write(snapshot: &Security, lexical: &Path, physical: &Path) -> Result<()> {
        snapshot
            .check_write(lexical)
            .and_then(|()| snapshot.check_write(physical))
            .map_err(OperationError::Failed)
    }

    fn regular(status: &libc::stat) -> Result<()> {
        if kind(status) != EntryKind::Regular {
            return Err(failed(
                FileReadErrorKind::InvalidOperation,
                "This operation requires a regular file.",
            ));
        }
        Ok(())
    }

    fn observation(lexical: PathBuf, physical: PathBuf, status: &libc::stat) -> FileMetadata {
        let hidden = lexical
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(b"."));
        FileMetadata {
            lexical_path: lexical,
            physical_path: physical,
            kind: kind(status),
            hidden,
        }
    }

    pub(super) fn inspect(
        path: &Path,
        links: LinkPolicy,
        snapshot: &Security,
    ) -> Result<FileMetadata> {
        #[cfg(target_os = "macos")]
        if links == LinkPolicy::NoFollow {
            let (physical, status) = current_entry(path, links)?;
            authorize_read(snapshot, path, &physical)?;
            let (current_physical, current) = current_entry(path, links).map_err(|_| changed())?;
            if current_physical != physical || !same(&status, &current) {
                return Err(changed());
            }
            return Ok(observation(path.to_path_buf(), physical, &status));
        }
        let descriptor = pin(path, links)?;
        let (physical, status) = validate(descriptor.as_raw_fd(), path, links, None)?;
        authorize_read(snapshot, path, &physical)?;
        Ok(observation(path.to_path_buf(), physical, &status))
    }

    #[cfg(target_os = "linux")]
    fn directory_entries(
        descriptor: RawFd,
    ) -> Result<impl Iterator<Item = std::io::Result<std::ffi::OsString>>> {
        Ok(fs::read_dir(format!("/proc/self/fd/{descriptor}"))
            .map_err(io)?
            .map(|entry| entry.map(|entry| entry.file_name())))
    }

    #[cfg(target_os = "macos")]
    struct DirectoryEntries(*mut libc::DIR);

    #[cfg(target_os = "macos")]
    impl Drop for DirectoryEntries {
        fn drop(&mut self) {
            // The stream is owned by this iterator and closed exactly once.
            unsafe { libc::closedir(self.0) };
        }
    }

    #[cfg(target_os = "macos")]
    impl Iterator for DirectoryEntries {
        type Item = std::io::Result<std::ffi::OsString>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                // readdir's entry is valid until the next call on this owned stream;
                // copy its NUL-terminated name before advancing. errno distinguishes EOF.
                let name = unsafe {
                    *libc::__error() = 0;
                    let entry = libc::readdir(self.0);
                    if entry.is_null() {
                        return if *libc::__error() == 0 {
                            None
                        } else {
                            Some(Err(std::io::Error::last_os_error()))
                        };
                    }
                    OsStr::from_bytes(std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes())
                        .to_os_string()
                };
                if name != "." && name != ".." {
                    return Some(Ok(name));
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn directory_entries(descriptor: RawFd) -> Result<DirectoryEntries> {
        use std::os::fd::IntoRawFd;

        // Duplicate so fdopendir owns its descriptor without consuming the validation anchor.
        let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        // fcntl returned a new owned descriptor. On failure fdopendir leaves it owned here.
        let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let stream = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
        if stream.is_null() {
            return Err(io(std::io::Error::last_os_error()));
        }
        let _ = duplicate.into_raw_fd();
        Ok(DirectoryEntries(stream))
    }

    pub(super) fn list_directory(
        path: &Path,
        links: LinkPolicy,
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<FileMetadata>> {
        let descriptor = pin(path, links)?;
        let (physical, status) = validate(descriptor.as_raw_fd(), path, links, None)?;
        if kind(&status) != EntryKind::Directory {
            return Err(failed(
                FileReadErrorKind::InvalidOperation,
                "Directory listing requires a directory.",
            ));
        }
        authorize_read(snapshot, path, &physical)?;
        let mut result = Vec::new();
        for entry in directory_entries(descriptor.as_raw_fd())? {
            check_cancel(cancellation)?;
            let name = entry.map_err(io)?;
            let status = match stat_at(descriptor.as_raw_fd(), &name, true) {
                Ok(value) => value,
                Err(OperationError::Failed(error)) if error.kind == FileReadErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let lexical_child = path.join(&name);
            let physical_child = physical.join(&name);
            if authorize_read(snapshot, &lexical_child, &physical_child).is_ok() {
                result.push(observation(lexical_child, physical_child, &status));
            }
        }
        validate(descriptor.as_raw_fd(), path, links, Some(&physical))?;
        Ok(result)
    }

    #[cfg(target_os = "linux")]
    fn reopen(pinned: &OwnedFd, writable: bool) -> Result<File> {
        let descriptor_path = format!("/proc/self/fd/{}", pinned.as_raw_fd());
        let descriptor = open_at(
            libc::AT_FDCWD,
            OsStr::new(&descriptor_path),
            (if writable {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            }) | libc::O_NONBLOCK,
            0,
            false,
        )?;
        if !same(&stat(pinned.as_raw_fd())?, &stat(descriptor.as_raw_fd())?) {
            return Err(changed());
        }
        Ok(File::from(descriptor))
    }

    #[cfg(target_os = "macos")]
    fn reopen(pinned: &OwnedFd, writable: bool) -> Result<File> {
        let physical = descriptor_path(pinned.as_raw_fd())?;
        let descriptor = open_at(
            libc::AT_FDCWD,
            physical.as_os_str(),
            (if writable {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            }) | libc::O_NONBLOCK,
            0,
            true,
        )?;
        // No bytes or truncation occur until the safely walked open matches the pin.
        if !same(&stat(pinned.as_raw_fd())?, &stat(descriptor.as_raw_fd())?) {
            return Err(changed());
        }
        Ok(File::from(descriptor))
    }

    fn bytes(
        file: &mut File,
        limit: Option<usize>,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            check_cancel(cancellation)?;
            let count = file.read(&mut buffer).map_err(io)?;
            if count == 0 {
                break;
            }
            if limit.is_some_and(|limit| result.len().saturating_add(count) > limit) {
                return Err(failed(
                    FileReadErrorKind::TooLarge,
                    "The file exceeds the permitted byte limit.",
                ));
            }
            result.extend_from_slice(&buffer[..count]);
        }
        Ok(result)
    }

    pub(super) fn read(
        path: &Path,
        request: &ReadRequest,
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<BoundRead> {
        let pinned = pin(path, request.links)?;
        let (physical, status) = validate(
            pinned.as_raw_fd(),
            path,
            request.links,
            request.expected_physical.as_deref(),
        )?;
        regular(&status)?;
        authorize_read(snapshot, path, &physical)?;
        check_cancel(cancellation)?;
        let mut file = reopen(&pinned, false)?;
        validate(file.as_raw_fd(), path, request.links, Some(&physical))?;
        authorize_read(snapshot, path, &physical)?;
        let bytes = bytes(&mut file, request.max_bytes, cancellation)?;
        validate(file.as_raw_fd(), path, request.links, Some(&physical))?;
        Ok(BoundRead {
            lexical_path: path.to_path_buf(),
            physical_path: physical,
            bytes,
        })
    }

    fn parent_names(path: &Path) -> Result<Vec<&OsStr>> {
        if path.file_name().is_none() {
            return Err(failed(
                FileReadErrorKind::InvalidPath,
                "A mutation destination must name a file.",
            ));
        }
        Ok(path
            .parent()
            .unwrap_or(Path::new("/"))
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect())
    }

    fn walk_parent(
        path: &Path,
        create: bool,
        inspect_missing: bool,
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<(OwnedFd, PathBuf)>> {
        let mut descriptor = open_at(libc::AT_FDCWD, OsStr::new("/"), DIRECTORY_FLAGS, 0, true)?;
        let mut current = PathBuf::from("/");
        let mut missing = false;
        for name in parent_names(path)? {
            check_cancel(cancellation)?;
            let next = current.join(name);
            if missing {
                authorize_write(snapshot, &next, &next)?;
                current = next;
                continue;
            }
            validate(
                descriptor.as_raw_fd(),
                &current,
                LinkPolicy::RejectLinks,
                Some(&current),
            )?;
            match open_at(descriptor.as_raw_fd(), name, DIRECTORY_FLAGS, 0, true) {
                Ok(next_descriptor) => descriptor = next_descriptor,
                Err(OperationError::Failed(error))
                    if error.kind == FileReadErrorKind::NotFound && inspect_missing =>
                {
                    authorize_write(snapshot, &next, &next)?;
                    missing = true;
                }
                Err(OperationError::Failed(error))
                    if error.kind == FileReadErrorKind::NotFound && create =>
                {
                    authorize_write(snapshot, &next, &next)?;
                    check_cancel(cancellation)?;
                    validate(
                        descriptor.as_raw_fd(),
                        &current,
                        LinkPolicy::RejectLinks,
                        Some(&current),
                    )?;
                    let name_c = c_path(name)?;
                    if unsafe { libc::mkdirat(descriptor.as_raw_fd(), name_c.as_ptr(), 0o777) } != 0
                    {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::EEXIST) {
                            return Err(io(error));
                        }
                    }
                    descriptor = open_at(descriptor.as_raw_fd(), name, DIRECTORY_FLAGS, 0, true)?;
                }
                Err(error) => return Err(error),
            }
            current = next;
        }
        if missing {
            return Ok(None);
        }
        validate(
            descriptor.as_raw_fd(),
            &current,
            LinkPolicy::RejectLinks,
            Some(&current),
        )?;
        Ok(Some((descriptor, current)))
    }

    pub(super) fn inspect_mutation(
        path: &Path,
        create: bool,
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<OwnedFd>> {
        check_cancel(cancellation)?;
        authorize_write(snapshot, path, path)?;
        match pin(path, LinkPolicy::RejectLinks) {
            Ok(descriptor) => {
                let (physical, status) = validate(
                    descriptor.as_raw_fd(),
                    path,
                    LinkPolicy::RejectLinks,
                    Some(path),
                )?;
                regular(&status)?;
                authorize_write(snapshot, path, &physical)?;
                Ok(Some(descriptor))
            }
            Err(OperationError::Failed(error))
                if error.kind == FileReadErrorKind::NotFound && create =>
            {
                walk_parent(path, false, true, snapshot, cancellation)?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn read_mutation(
        path: &Path,
        pinned: OwnedFd,
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<u8>> {
        let mut file = reopen(&pinned, false)?;
        let (physical, status) =
            validate(file.as_raw_fd(), path, LinkPolicy::RejectLinks, Some(path))?;
        regular(&status)?;
        authorize_write(snapshot, path, &physical)?;
        let bytes = bytes(&mut file, None, cancellation)?;
        validate(file.as_raw_fd(), path, LinkPolicy::RejectLinks, Some(path))?;
        Ok(bytes)
    }

    fn create_file(directory: RawFd, name: &OsStr) -> std::io::Result<OwnedFd> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "A filesystem path contains a null byte.",
            )
        })?;
        let descriptor = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    pub(super) fn write(
        path: &Path,
        create: bool,
        data: &[u8],
        snapshot: &Security,
        cancellation: &dyn Cancellation,
    ) -> Result<()> {
        inspect_mutation(path, create, snapshot, cancellation)?;
        let (parent, parent_path) =
            walk_parent(path, create, false, snapshot, cancellation)?.ok_or_else(changed)?;
        let name = path.file_name().ok_or_else(|| {
            failed(
                FileReadErrorKind::InvalidPath,
                "A mutation destination must name a file.",
            )
        })?;
        check_cancel(cancellation)?;
        validate(
            parent.as_raw_fd(),
            &parent_path,
            LinkPolicy::RejectLinks,
            Some(&parent_path),
        )?;
        authorize_write(snapshot, path, path)?;
        let mut file = match open_at(parent.as_raw_fd(), name, PIN_FLAGS, 0, true) {
            Ok(pinned) => {
                regular(&stat(pinned.as_raw_fd())?)?;
                reopen(&pinned, true)?
            }
            Err(OperationError::Failed(error))
                if error.kind == FileReadErrorKind::NotFound && create =>
            {
                match create_file(parent.as_raw_fd(), name) {
                    Ok(descriptor) => File::from(descriptor),
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                        let pinned = open_at(parent.as_raw_fd(), name, PIN_FLAGS, 0, true)?;
                        regular(&stat(pinned.as_raw_fd())?)?;
                        reopen(&pinned, true)?
                    }
                    Err(error) => return Err(io(error)),
                }
            }
            Err(error) => return Err(error),
        };
        let (physical, status) =
            validate(file.as_raw_fd(), path, LinkPolicy::RejectLinks, Some(path))?;
        regular(&status)?;
        validate(
            parent.as_raw_fd(),
            &parent_path,
            LinkPolicy::RejectLinks,
            Some(&parent_path),
        )?;
        if !same(&status, &stat_at(parent.as_raw_fd(), name, true)?) {
            return Err(changed());
        }
        authorize_write(snapshot, path, &physical)?;
        check_cancel(cancellation)?;
        file.set_len(0).map_err(io)?;
        for chunk in data.chunks(8192) {
            check_cancel(cancellation)?;
            file.write_all(chunk).map_err(io)?;
        }
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::super::policy::ProductionPolicyMaterializer;
    use super::*;
    use parrot_domain::contracts::{filesystem::EntryKind, security::AccessSnapshot};
    use parrot_foundation::{
        config::runtime::{SandboxRule, SandboxRuleAction},
        paths::AgentScratchDirectory,
    };
    use parrot_runtime::plan_mode::RetainedModePolicyMaterializer;
    use parrot_storage::events::NeverCancelled;
    use std::{
        fs,
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, PermissionsExt, symlink},
        },
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        workspace: ProjectWorkspace,
        filesystem: ProductionFilesystem,
        session: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "parrot-fs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let work = root.join("work");
            fs::create_dir_all(&work).unwrap();
            // macOS temporary-directory aliases must not become mutation symlink parents.
            #[cfg(target_os = "macos")]
            let work = fs::canonicalize(work).unwrap();
            #[cfg(target_os = "macos")]
            let root = work.parent().unwrap().to_path_buf();
            let workspace = ProjectWorkspace::from_resolved(work.clone(), work, None, None);
            let filesystem = ProductionFilesystem::new(workspace.clone()).unwrap();
            Self {
                session: root.join("session"),
                root,
                workspace,
                filesystem,
            }
        }
        fn security(&self, rules: &[SandboxRule]) -> Security {
            let materializer = ProductionPolicyMaterializer::new();
            materializer
                .register_session(self.session.clone(), self.workspace.clone())
                .unwrap();
            let scratch =
                AgentScratchDirectory::new(&self.session.join("root-agents/main"), &self.root)
                    .unwrap();
            let raw = Arc::new(ProductionPolicy::compose(false, &[], rules, &[]).unwrap());
            SharedSecurity(materializer.for_agent(&raw, &scratch).unwrap())
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.workspace.launch_directory().join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn rule(path: &Path, action: SandboxRuleAction) -> SandboxRule {
        SandboxRule {
            path: path.to_string_lossy().into_owned(),
            action,
        }
    }
    fn read_request(path: &Path, links: LinkPolicy) -> ReadRequest {
        ReadRequest {
            requested: path.to_path_buf(),
            expected_physical: None,
            links,
            max_bytes: None,
        }
    }
    fn error_kind<T>(result: Result<T>) -> FileReadErrorKind {
        match result {
            Err(OperationError::Failed(error)) => error.kind,
            _ => panic!("expected filesystem failure"),
        }
    }

    #[tokio::test]
    async fn allowed_reads_denials_and_actual_byte_limits() {
        let fixture = Fixture::new();
        let path = fixture.file("read.txt", b"hello");
        let security = fixture.security(&[]);
        let result = fixture
            .filesystem
            .read(
                &read_request(Path::new("read.txt"), LinkPolicy::Follow),
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(result.bytes, b"hello");
        assert_eq!(result.physical_path, path);
        let denied = fixture.security(&[rule(&path, SandboxRuleAction::DenyRead)]);
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(
                        &read_request(&path, LinkPolicy::Follow),
                        &denied,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::Denied
        );
        let mut request = read_request(&path, LinkPolicy::Follow);
        request.max_bytes = Some(4);
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(&request, &security, &NeverCancelled)
                    .await
            ),
            FileReadErrorKind::TooLarge
        );
        request.max_bytes = Some(5);
        assert_eq!(
            fixture
                .filesystem
                .read(&request, &security, &NeverCancelled)
                .await
                .unwrap()
                .bytes,
            b"hello"
        );
    }

    #[tokio::test]
    async fn symlinked_workspace_retains_lexical_and_physical_authorization() {
        let fixture = Fixture::new();
        let physical_file = fixture.file("file", b"content");
        let alias = fixture.root.join("workspace-alias");
        symlink(fixture.workspace.launch_directory(), &alias).unwrap();
        let filesystem = ProductionFilesystem::new(ProjectWorkspace::from_resolved(
            alias.clone(),
            fixture.workspace.physical_identity().to_path_buf(),
            None,
            None,
        ))
        .unwrap();
        let request = read_request(Path::new("file"), LinkPolicy::Follow);
        let allowed = fixture.security(&[rule(&alias, SandboxRuleAction::AllowWrite)]);
        let read = filesystem
            .read(&request, &allowed, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(read.lexical_path, alias.join("file"));
        assert_eq!(read.physical_path, physical_file);
        for denied_path in [&alias, fixture.workspace.launch_directory()] {
            let denied = fixture.security(&[rule(denied_path, SandboxRuleAction::DenyRead)]);
            assert_eq!(
                error_kind(filesystem.read(&request, &denied, &NeverCancelled).await),
                FileReadErrorKind::Denied
            );
        }
        assert_eq!(
            error_kind(
                filesystem
                    .read(
                        &read_request(Path::new("file"), LinkPolicy::RejectLinks),
                        &allowed,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::InvalidOperation
        );
        assert_eq!(
            error_kind(
                filesystem
                    .write_for_mutation(
                        &MutationRequest {
                            requested: "file".into(),
                            create: false
                        },
                        b"bad",
                        &allowed,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::InvalidOperation
        );
        assert_eq!(fs::read(&physical_file).unwrap(), b"content");
    }

    #[tokio::test]
    async fn expected_target_is_checked_and_final_nofollow_is_observed() {
        let fixture = Fixture::new();
        let path = fixture.file("file", b"x");
        let link = fixture.workspace.launch_directory().join("link");
        symlink(&path, &link).unwrap();
        let security = fixture.security(&[]);
        let meta = fixture
            .filesystem
            .inspect(&link, LinkPolicy::NoFollow, &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(meta.kind, EntryKind::SymbolicLink);
        assert_eq!(meta.physical_path, link);
        let mut request = read_request(&path, LinkPolicy::Follow);
        request.expected_physical = Some(fixture.root.join("different"));
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(&request, &security, &NeverCancelled)
                    .await
            ),
            FileReadErrorKind::TargetChanged
        );
    }

    #[tokio::test]
    async fn follow_cannot_escape_denied_physical_target_and_reject_links_checks_components() {
        let fixture = Fixture::new();
        let outside = fixture.root.join("private");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        let link = fixture.workspace.launch_directory().join("linked");
        symlink(&outside, &link).unwrap();
        let security = fixture.security(&[rule(&outside, SandboxRuleAction::DenyRead)]);
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(
                        &read_request(&link.join("secret"), LinkPolicy::Follow),
                        &security,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::Denied
        );
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(
                        &read_request(&link.join("secret"), LinkPolicy::RejectLinks),
                        &security,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::InvalidOperation
        );
    }

    #[tokio::test]
    async fn listings_do_not_follow_final_links_or_disclose_denied_names() {
        let fixture = Fixture::new();
        fixture.file("visible", b"x");
        let hidden = fixture.file("denied", b"x");
        symlink(&hidden, fixture.workspace.launch_directory().join("link")).unwrap();
        let security = fixture.security(&[rule(&hidden, SandboxRuleAction::DenyRead)]);
        let entries = fixture
            .filesystem
            .list_directory(
                Path::new("."),
                LinkPolicy::Follow,
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert!(!entries.iter().any(|entry| entry.lexical_path == hidden));
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == EntryKind::SymbolicLink
                    && entry.lexical_path.ends_with("link"))
        );
    }

    #[tokio::test]
    async fn mutation_inspection_is_effect_free_and_missing_edit_fails() {
        let fixture = Fixture::new();
        let security = fixture.security(&[]);
        let path = fixture
            .workspace
            .launch_directory()
            .join("new/parents/file");
        let request = MutationRequest {
            requested: path.clone(),
            create: true,
        };
        let target = fixture
            .filesystem
            .inspect_for_mutation(&request, &security, &NeverCancelled)
            .await
            .unwrap();
        assert_eq!(target.physical_path, path);
        assert_eq!(target.display_path, "new/parents/file");
        assert!(!path.parent().unwrap().exists());
        assert!(
            fixture
                .filesystem
                .read_for_mutation(&request, &security, &NeverCancelled)
                .await
                .unwrap()
                .bytes
                .is_none()
        );
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .inspect_for_mutation(
                        &MutationRequest {
                            create: false,
                            ..request
                        },
                        &security,
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn denied_missing_parent_prevents_every_creation() {
        let fixture = Fixture::new();
        let parent = fixture.workspace.launch_directory().join("blocked");
        let path = parent.join("child/file");
        let security = fixture.security(&[
            rule(&parent, SandboxRuleAction::DenyWrite),
            rule(&path, SandboxRuleAction::AllowWrite),
        ]);
        assert!(security.check_write(&path).is_ok());
        let result = fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: path,
                    create: true,
                },
                b"x",
                &security,
                &NeverCancelled,
            )
            .await;
        assert_eq!(error_kind(result), FileReadErrorKind::Denied);
        assert!(!parent.exists());
    }

    #[tokio::test]
    async fn overwrite_preserves_mode_and_inode_and_creation_is_private() {
        let fixture = Fixture::new();
        let path = fixture.file("existing", b"old contents");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let security = fixture.security(&[]);
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: path.clone(),
                    create: false,
                },
                b"new",
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(after.mode() & 0o777, 0o640);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let created = fixture.workspace.launch_directory().join("a/b/new");
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: created.clone(),
                    create: true,
                },
                b"private",
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(fs::metadata(&created).unwrap().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn write_policy_not_read_policy_controls_mutation_reads() {
        let fixture = Fixture::new();
        let security = fixture.security(&[rule(&fixture.session, SandboxRuleAction::DenyRead)]);
        let path = fixture.session.join("owned");
        fs::write(&path, b"owned").unwrap();
        assert!(!security.snapshot().allows_read(&path));
        assert!(security.snapshot().allows_write(&path));
        assert_eq!(
            fixture
                .filesystem
                .read_for_mutation(
                    &MutationRequest {
                        requested: path.clone(),
                        create: false
                    },
                    &security,
                    &NeverCancelled
                )
                .await
                .unwrap()
                .bytes
                .unwrap(),
            b"owned"
        );
        fixture
            .filesystem
            .write_for_mutation(
                &MutationRequest {
                    requested: path.clone(),
                    create: false,
                },
                b"updated",
                &security,
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"updated");
    }

    #[tokio::test]
    async fn mutation_rejects_final_and_intermediate_links_without_touching_targets() {
        let fixture = Fixture::new();
        let path = fixture.file("target", b"original");
        let link = fixture.workspace.launch_directory().join("link");
        symlink(&path, &link).unwrap();
        let security = fixture.security(&[]);
        assert!(
            fixture
                .filesystem
                .write_for_mutation(
                    &MutationRequest {
                        requested: link,
                        create: true
                    },
                    b"bad",
                    &security,
                    &NeverCancelled
                )
                .await
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let directory_link = fixture.workspace.launch_directory().join("directory-link");
        symlink(fixture.workspace.launch_directory(), &directory_link).unwrap();
        assert!(
            fixture
                .filesystem
                .write_for_mutation(
                    &MutationRequest {
                        requested: directory_link.join("new"),
                        create: true
                    },
                    b"bad",
                    &security,
                    &NeverCancelled
                )
                .await
                .is_err()
        );
        assert!(!fixture.workspace.launch_directory().join("new").exists());
    }

    #[tokio::test]
    async fn preflight_then_swapped_link_is_rejected_before_truncation() {
        let fixture = Fixture::new();
        let path = fixture.file("original", b"original");
        let victim = fixture.file("victim", b"victim");
        let security = fixture.security(&[]);
        let request = MutationRequest {
            requested: path.clone(),
            create: false,
        };
        fixture
            .filesystem
            .inspect_for_mutation(&request, &security, &NeverCancelled)
            .await
            .unwrap();
        let moved = fixture.workspace.launch_directory().join("moved");
        fs::rename(&path, &moved).unwrap();
        symlink(&victim, &path).unwrap();
        assert!(
            fixture
                .filesystem
                .write_for_mutation(&request, b"bad", &security, &NeverCancelled)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"victim");
        assert_eq!(fs::read(&moved).unwrap(), b"original");
    }

    #[tokio::test]
    async fn fifo_read_is_rejected_without_opening_a_blocking_reader() {
        let fixture = Fixture::new();
        let path = fixture.workspace.launch_directory().join("fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(
            error_kind(
                fixture
                    .filesystem
                    .read(
                        &read_request(&path, LinkPolicy::Follow),
                        &fixture.security(&[]),
                        &NeverCancelled
                    )
                    .await
            ),
            FileReadErrorKind::InvalidOperation
        );
    }

    #[test]
    fn opened_target_replacement_is_detected_without_reading_the_replacement() {
        let fixture = Fixture::new();
        let path = fixture.file("original", b"old");
        let descriptor = unix_fs::pin(&path, LinkPolicy::RejectLinks).unwrap();
        fs::rename(&path, fixture.workspace.launch_directory().join("moved")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(
            error_kind(unix_fs::validate(
                descriptor.as_raw_fd(),
                &path,
                LinkPolicy::RejectLinks,
                Some(&path)
            )),
            FileReadErrorKind::TargetChanged
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }
}
