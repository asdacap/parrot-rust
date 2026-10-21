use super::{Error, ErrorKind, Result, SessionMeta};
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::values::identifiers::UserSessionId;
use std::fs;
use std::path::PathBuf;

/// The selected session's metadata index. Publishing never opens the database.
#[derive(Clone, Debug)]
pub struct SessionIndex {
    root: SelectedSessionRoot,
    id: UserSessionId,
    launch_directory: PathBuf,
    metadata_path: PathBuf,
}

impl SessionIndex {
    pub fn new(
        root: SelectedSessionRoot,
        id: UserSessionId,
        launch_directory: impl Into<PathBuf>,
    ) -> Result<Self> {
        let launch_directory = launch_directory.into();
        if !launch_directory.is_absolute() {
            return Err(Error::invalid_input(
                "session workspace must be an absolute launch directory",
            ));
        }
        if root.path().file_name().and_then(|name| name.to_str()) != Some(id.value()) {
            return Err(Error::invalid_input(
                "selected session directory does not match session id",
            ));
        }
        Ok(Self {
            metadata_path: root.path().join("meta.json"),
            root,
            id,
            launch_directory,
        })
    }

    pub fn metadata_path(&self) -> &std::path::Path {
        &self.metadata_path
    }

    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }

    /// Missing, linked, malformed or foreign-session metadata is not an index hit.
    pub fn find(&self) -> Result<Option<SessionMeta>> {
        if !self.metadata_path.is_file()
            || is_symlink(self.root.path())
            || is_symlink(&self.metadata_path)
        {
            return Ok(None);
        }
        let contents = fs::read_to_string(&self.metadata_path)
            .map_err(|error| Error::io("read session metadata", error))?;
        let metadata: Option<SessionMeta> = serde_json::from_str(&contents)
            .map_err(|error| Error::invalid_data(format!("invalid session metadata: {error}")))?;
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        if self.owns(&metadata) {
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    }

    /// Stage a complete PascalCase document and rename it over meta.json.
    pub fn publish(&self, metadata: &SessionMeta) -> Result<()> {
        if !self.owns(metadata) {
            return Err(Error::new(
                ErrorKind::Unowned,
                "Session metadata does not belong to these resources.",
            ));
        }
        if is_symlink(self.root.path()) || is_symlink(&self.metadata_path) {
            return Err(Error::new(
                ErrorKind::Unowned,
                "Session metadata paths cannot be symbolic links.",
            ));
        }
        fs::create_dir_all(self.root.path())
            .map_err(|error| Error::io("create session metadata directory", error))?;
        let staging = append_suffix(&self.metadata_path, ".staging");
        let serialized = super::json::serialize_metadata(metadata)?;
        fs::write(&staging, serialized)
            .map_err(|error| Error::io("write staged session metadata", error))?;
        #[cfg(test)]
        stage_crash_tests::after_full_stage_write(self, &staging);
        fs::rename(&staging, &self.metadata_path)
            .map_err(|error| Error::io("publish staged session metadata", error))
    }

    fn owns(&self, metadata: &SessionMeta) -> bool {
        metadata
            .id
            .as_deref()
            .and_then(UserSessionId::try_parse)
            .as_ref()
            == Some(&self.id)
            && metadata
                .working_directory
                .as_deref()
                .is_some_and(|path| self.owns_workspace(path))
    }

    fn owns_workspace(&self, working_directory: &str) -> bool {
        if working_directory == self.launch_directory.as_os_str().to_string_lossy() {
            return true;
        }
        let given = PathBuf::from(working_directory);
        match (given.canonicalize(), self.launch_directory.canonicalize()) {
            (Ok(given), Ok(expected)) => given == expected,
            _ => false,
        }
    }
}

fn is_symlink(path: &std::path::Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn append_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod stage_crash_tests;
