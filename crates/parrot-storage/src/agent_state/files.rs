use parrot_domain::values::agent_state::{AGENT_STATE_FILE_NAME, AGENT_STATE_STAGING_FILE_NAME};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, statat, unlinkat};

pub(super) struct AgentStateDirectory {
    #[cfg(unix)]
    directory: File,
    #[cfg(not(unix))]
    path: std::path::PathBuf,
}
impl AgentStateDirectory {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("agent directory must be absolute"));
        }
        #[cfg(unix)]
        {
            use std::path::Component;
            let mut directory = File::open("/")?;
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        directory = openat(
                            &directory,
                            name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map(File::from)
                        .map_err(invalid_path)?;
                    }
                    _ => {
                        return Err(invalid(
                            "agent directory must not contain relative components",
                        ));
                    }
                }
            }
            use std::os::unix::fs::PermissionsExt;
            if directory.metadata()?.permissions().mode() & 0o077 != 0 {
                return Err(invalid("agent directory must be private"));
            }
            Ok(Self { directory })
        }
        #[cfg(not(unix))]
        {
            validate_directory(path)?;
            Ok(Self { path: path.into() })
        }
    }

    pub(super) fn exists(&self) -> io::Result<bool> {
        #[cfg(unix)]
        let result = statat(
            &self.directory,
            AGENT_STATE_FILE_NAME,
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map(|_| ())
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        let result = std::fs::symlink_metadata(self.path.join(AGENT_STATE_FILE_NAME)).map(|_| ());
        match result {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(super) fn retire(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            for name in [AGENT_STATE_FILE_NAME, AGENT_STATE_STAGING_FILE_NAME] {
                match unlinkat(&self.directory, name, AtFlags::empty()) {
                    Ok(()) => {}
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.directory.sync_all()
        }
        #[cfg(not(unix))]
        {
            for name in [AGENT_STATE_FILE_NAME, AGENT_STATE_STAGING_FILE_NAME] {
                match std::fs::remove_file(self.path.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
    }

    pub(super) fn read(&self) -> io::Result<Option<Vec<u8>>> {
        #[cfg(unix)]
        let result = openat(
            &self.directory,
            AGENT_STATE_FILE_NAME,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        let result = self.open_file(AGENT_STATE_FILE_NAME);
        let mut file = match result {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(invalid_path(error)),
        };
        if !file.metadata()?.is_file() {
            return Err(invalid("agent snapshot must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub(super) fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        self.check_replace_target()?;
        self.remove_staging()?;
        let result = (|| {
            let mut file = self.create_staging()?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            self.commit_staging()
        })();
        if result.is_err() {
            let _ = self.remove_staging();
        }
        result
    }

    fn check_replace_target(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let metadata = match statat(
                &self.directory,
                AGENT_STATE_FILE_NAME,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(metadata) => metadata,
                Err(rustix::io::Errno::NOENT) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                != rustix::fs::FileType::RegularFile
            {
                return Err(invalid("agent snapshot must be a regular file"));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            match self.open_file(AGENT_STATE_FILE_NAME) {
                Ok(_) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    fn remove_staging(&self) -> io::Result<()> {
        #[cfg(unix)]
        let result = unlinkat(
            &self.directory,
            AGENT_STATE_STAGING_FILE_NAME,
            AtFlags::empty(),
        )
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        let result = std::fs::remove_file(self.path.join(AGENT_STATE_STAGING_FILE_NAME));
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn create_staging(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            openat(
                &self.directory,
                AGENT_STATE_STAGING_FILE_NAME,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map(File::from)
            .map_err(io::Error::from)
        }
        #[cfg(not(unix))]
        {
            validate_directory(&self.path)?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(AGENT_STATE_STAGING_FILE_NAME))
        }
    }

    fn commit_staging(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            renameat(
                &self.directory,
                AGENT_STATE_STAGING_FILE_NAME,
                &self.directory,
                AGENT_STATE_FILE_NAME,
            )?;
            self.directory.sync_all()
        }
        #[cfg(not(unix))]
        {
            validate_directory(&self.path)?;
            std::fs::rename(
                self.path.join(AGENT_STATE_STAGING_FILE_NAME),
                self.path.join(AGENT_STATE_FILE_NAME),
            )
        }
    }

    #[cfg(not(unix))]
    fn open_file(&self, name: &str) -> io::Result<File> {
        validate_directory(&self.path)?;
        let path = self.path.join(name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid("agent snapshot must be a regular file, not a link"));
        }
        File::open(path)
    }
}

#[cfg(not(unix))]
fn validate_directory(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("agent directory must be a directory, not a link"));
        }
    }
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn invalid_path(error: impl Into<io::Error>) -> io::Error {
    let error = error.into();
    #[cfg(unix)]
    if matches!(
        rustix::io::Errno::from_io_error(&error),
        Some(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR)
    ) {
        return invalid(&error.to_string());
    }
    error
}
