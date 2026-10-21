use std::{
    fs::{self, DirBuilder, Permissions},
    io,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::{SocketAddr, UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixSocketIdentity {
    pub device: u64,
    pub inode: u64,
}

impl UnixSocketIdentity {
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix socket transport requires Linux or macOS",
            ));
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "transport path exists and is not a socket",
            ));
        }
        #[cfg(target_os = "linux")]
        let device =
            (u64::from(libc::major(metadata.dev())) << 32) | u64::from(libc::minor(metadata.dev()));
        #[cfg(not(target_os = "linux"))]
        let device = metadata.dev() & u64::from(u32::MAX);
        Ok(Some(Self {
            device,
            inode: metadata.ino(),
        }))
    }

    /// Matches source pathname identity checking; the read and unlink are not atomic.
    pub fn remove_owned(&self, path: &Path) -> io::Result<bool> {
        let current = match Self::read(path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => return Ok(false),
            Err(error) => return Err(error),
        };
        if current != Some(*self) {
            return Ok(false);
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug)]
pub struct OwnedUnixSocket {
    path: PathBuf,
    identity: UnixSocketIdentity,
}

impl OwnedUnixSocket {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> UnixSocketIdentity {
        self.identity
    }

    pub fn cleanup(&self) -> io::Result<bool> {
        self.identity.remove_owned(&self.path)
    }
}

impl Drop for OwnedUnixSocket {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub fn bind(path: &Path) -> io::Result<(UnixListener, OwnedUnixSocket)> {
    // Validate before creating/chmodding any directory, as the source endpoint does.
    SocketAddr::from_pathname(path)?;
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a Unix transport needs an absolute socket path",
        ));
    }
    prepare_control_directory(path)?;
    if let Some(existing) = UnixSocketIdentity::read(path)? {
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "transport socket is already active",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                existing.remove_owned(path)?;
            }
            Err(error) => return Err(error),
        }
    }
    let listener = UnixListener::bind(path)?;
    let identity = UnixSocketIdentity::read(path)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "the bound Unix socket disappeared")
    })?;
    let owned = OwnedUnixSocket {
        path: path.to_owned(),
        identity,
    };
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok((listener, owned))
}

fn prepare_control_directory(path: &Path) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the Unix socket has no parent")
    })?;
    for ancestor in directory.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unix socket directories cannot be symbolic links",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    fs::set_permissions(directory, Permissions::from_mode(0o700))
}
