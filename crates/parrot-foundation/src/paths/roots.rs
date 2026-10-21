use super::{StatePaths, invalid, native_platform, normalize};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::values::identifiers::UserSessionId;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRoots {
    current: StatePaths,
    legacy: StatePaths,
    user_home: PathBuf,
}

impl ApplicationRoots {
    pub fn resolve(environment: &HashMap<String, String>) -> Self {
        Self {
            current: StatePaths::resolve(environment),
            legacy: StatePaths::for_application(environment, "parrotdotnet"),
            user_home: environment
                .get("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
        }
    }

    pub fn current(&self) -> &StatePaths {
        &self.current
    }
    pub fn legacy(&self) -> &StatePaths {
        &self.legacy
    }
    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    /// Selects the new application root without creating a session or acquiring
    /// its ownership lease. Fresh-ID collision/admission remains storage-owned.
    pub fn select_new(
        &self,
        id: &UserSessionId,
        current_directory: &Path,
    ) -> io::Result<SelectedSessionRoot> {
        let path = self.session_path(&self.current, id, current_directory)?;
        validate_session_directory(&path)?;
        selected(path, RootOrigin::Current)
    }

    /// Directory presence selects the root before metadata is loaded. A corrupt
    /// or denied current session is not retried in the legacy root by consumers.
    pub fn discover_session(
        &self,
        id: &UserSessionId,
        legacy_enabled: bool,
        current_directory: &Path,
    ) -> io::Result<Option<SelectedSessionRoot>> {
        let current = self.session_path(&self.current, id, current_directory)?;
        if validate_session_directory(&current)? {
            return selected(current, RootOrigin::Current).map(Some);
        }
        if legacy_enabled {
            let legacy = self.session_path(&self.legacy, id, current_directory)?;
            if validate_session_directory(&legacy)? {
                return selected(legacy, RootOrigin::Legacy).map(Some);
            }
        }
        Ok(None)
    }

    fn session_path(
        &self,
        paths: &StatePaths,
        id: &UserSessionId,
        current_directory: &Path,
    ) -> io::Result<PathBuf> {
        normalize(
            &paths.state().join("sessions").join(id.value()),
            current_directory,
            native_platform(),
        )
    }
}

fn validate_session_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session root must be a directory, not a symbolic link or file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn selected(path: PathBuf, origin: RootOrigin) -> io::Result<SelectedSessionRoot> {
    // Materialize existing ancestor links even when a new session does not yet
    // exist. This fixes the token's physical root, not a filesystem permission.
    let mut existing = path.as_path();
    let mut missing = Vec::new();
    let mut physical = loop {
        match fs::canonicalize(existing) {
            Ok(path) => break path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(existing)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(error);
                }
                let name = existing.file_name().ok_or(error)?;
                missing.push(name.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| invalid("session root has no parent"))?;
            }
            Err(error) => return Err(error),
        }
    };
    for part in missing.into_iter().rev() {
        physical.push(part);
    }
    SelectedSessionRoot::new(physical, origin).map_err(|error| invalid(&error.to_string()))
}
