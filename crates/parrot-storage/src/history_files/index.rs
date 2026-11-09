use super::{Error, ErrorKind, Result, SessionMeta};
use crate::conversation::PendingStatus;
use crate::ownership::SessionModelSelection;
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::values::{identifiers::UserSessionId, lineage::AgentLineageRecord};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIdentityState {
    pub root_agent_session_id: String,
    pub root_agent_name: String,
    pub mode: PendingStatus,
}

/// Owns canonical session identity and preferences without opening the database.
#[derive(Clone, Debug)]
pub struct SessionIndex {
    shared: Arc<SessionIndexState>,
}

#[derive(Debug)]
struct SessionIndexState {
    root: SelectedSessionRoot,
    id: UserSessionId,
    launch_directory: PathBuf,
    metadata_path: PathBuf,
    metadata_gate: Mutex<()>,
    mode_gate: Arc<AsyncMutex<()>>,
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
            shared: Arc::new(SessionIndexState {
                metadata_path: root.path().join("meta.json"),
                root,
                id,
                launch_directory,
                metadata_gate: Mutex::new(()),
                mode_gate: Arc::new(AsyncMutex::new(())),
            }),
        })
    }

    pub fn metadata_path(&self) -> &std::path::Path {
        &self.shared.metadata_path
    }

    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.shared.root
    }

    /// Missing, linked, malformed or foreign-session metadata is not an index hit.
    pub fn find(&self) -> Result<Option<SessionMeta>> {
        if !self.metadata_path().is_file()
            || is_symlink(self.selected_root().path())
            || is_symlink(self.metadata_path())
        {
            return Ok(None);
        }
        let contents = fs::read_to_string(self.metadata_path())
            .map_err(|error| Error::io("read session metadata", error))?;
        let metadata: Option<SessionMeta> = serde_json::from_str(&contents)
            .map_err(|error| Error::invalid_data(format!("invalid session metadata: {error}")))?;
        Ok(metadata.filter(|metadata| self.owns(metadata)))
    }

    /// Creates canonical identity before history can be written. Never replaces metadata.
    pub fn initialize(
        &self,
        mut metadata: SessionMeta,
        root_agent_session_id: String,
    ) -> Result<()> {
        let _guard = self
            .shared
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        if !self.owns(&metadata) {
            return Err(Error::new(
                ErrorKind::Unowned,
                "Session metadata does not belong to these resources.",
            ));
        }
        match fs::symlink_metadata(self.metadata_path()) {
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidOperation,
                    "Session metadata already exists.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io("inspect session metadata", error)),
        }
        metadata.agent_lineage = Some(vec![AgentLineageRecord::new(
            &root_agent_session_id,
            "",
            metadata.root_agent_name.as_deref().unwrap_or_default(),
        )]);
        metadata.root_agent_session_id = Some(root_agent_session_id);
        metadata.mode_revision = Some(0);
        active_state(&metadata)?;
        self.publish(&metadata)
    }

    pub fn read_session_state(&self) -> Result<SessionIdentityState> {
        let _guard = self
            .shared
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        active_state(&self.require_metadata()?)
    }

    pub fn read_agent_lineage(&self) -> Result<Vec<AgentLineageRecord>> {
        let _guard = self
            .shared
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        let metadata = self.require_metadata()?;
        active_state(&metadata)?;
        metadata
            .agent_lineage
            .ok_or_else(|| Error::invalid_data("session lineage is missing"))
    }

    /// Remembers immutable membership, including retired incarnations of a directory.
    pub fn remember_agent(&self, record: AgentLineageRecord) -> Result<()> {
        self.update(|metadata| {
            let records = metadata.agent_lineage.as_mut().expect("validated lineage");
            if let Some(existing) = records
                .iter()
                .find(|existing| existing.session_id == record.session_id)
            {
                return if existing == &record {
                    Ok(false)
                } else {
                    Err(Error::invalid_data(
                        "agent identity conflicts with permanent session lineage",
                    ))
                };
            }
            records.push(record);
            active_state(metadata)?;
            Ok(true)
        })
    }

    pub async fn update_mode(&self, mode: &str) -> Result<()> {
        let _guard = self.shared.mode_gate.clone().lock_owned().await;
        self.update(|metadata| {
            if mode.trim().is_empty() {
                return Err(Error::invalid_input("session mode must not be empty"));
            }
            let revision = metadata
                .mode_revision
                .expect("validated revision")
                .checked_add(1)
                .ok_or_else(|| Error::invalid_data("mode revision overflow"))?;
            metadata.mode = Some(mode.into());
            metadata.mode_revision = Some(revision);
            Ok(true)
        })
    }

    pub fn update_model_selection(&self, model: &SessionModelSelection) -> Result<()> {
        self.update(|metadata| {
            metadata.provider_id = Some(model.provider_id.clone());
            metadata.model = Some(model.canonical_model.clone());
            metadata.selector = Some(model.selector.clone());
            Ok(true)
        })
    }

    pub fn update_task_limit(&self, limit: i32) -> Result<()> {
        if limit < 0 {
            return Err(Error::invalid_input("task limit must be nonnegative"));
        }
        self.update(|metadata| {
            metadata.extra_concurrent_tasks = Some(limit);
            Ok(true)
        })
    }

    pub fn update_fast_mode(&self, enabled: bool) -> Result<()> {
        self.update(|metadata| {
            metadata.fast_mode = Some(enabled);
            Ok(true)
        })
    }

    pub fn record_opened(&self, opened_at: &str) -> Result<()> {
        self.update(|metadata| {
            metadata.last_opened_at = Some(opened_at.into());
            Ok(true)
        })
    }

    pub(crate) async fn guard_status(
        &self,
        agent: &str,
        expected: &PendingStatus,
    ) -> Result<Option<OwnedMutexGuard<()>>> {
        let guard = self.shared.mode_gate.clone().lock_owned().await;
        let state = self.read_session_state()?;
        Ok((state.root_agent_session_id == agent && state.mode == *expected).then_some(guard))
    }

    fn require_metadata(&self) -> Result<SessionMeta> {
        self.find()?.ok_or_else(|| {
            Error::invalid_data(
                "session metadata is missing, linked, or belongs to another session",
            )
        })
    }

    fn update(&self, operation: impl FnOnce(&mut SessionMeta) -> Result<bool>) -> Result<()> {
        let _guard = self
            .shared
            .metadata_gate
            .lock()
            .expect("session metadata poisoned");
        let mut metadata = self.require_metadata()?;
        active_state(&metadata)?;
        if operation(&mut metadata)? {
            self.publish(&metadata)?;
        }
        Ok(())
    }

    pub(super) fn publish(&self, metadata: &SessionMeta) -> Result<()> {
        if !self.owns(metadata) {
            return Err(Error::new(
                ErrorKind::Unowned,
                "Session metadata does not belong to these resources.",
            ));
        }
        if is_symlink(self.selected_root().path()) || is_symlink(self.metadata_path()) {
            return Err(Error::new(
                ErrorKind::Unowned,
                "Session metadata paths cannot be symbolic links.",
            ));
        }
        fs::create_dir_all(self.selected_root().path())
            .map_err(|error| Error::io("create session metadata directory", error))?;
        let staging = append_suffix(self.metadata_path(), ".staging");
        let serialized = super::json::serialize_metadata(metadata)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match fs::remove_file(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::io(
                    "remove stale session metadata staging file",
                    error,
                ));
            }
        }
        let mut staged = options
            .open(&staging)
            .map_err(|error| Error::io("create staged session metadata", error))?;
        staged
            .write_all(&serialized)
            .and_then(|()| staged.sync_all())
            .map_err(|error| Error::io("write staged session metadata", error))?;
        drop(staged);
        #[cfg(test)]
        stage_crash_tests::after_full_stage_write(self, &staging);
        fs::rename(&staging, self.metadata_path())
            .map_err(|error| Error::io("publish staged session metadata", error))?;
        #[cfg(unix)]
        File::open(self.selected_root().path())
            .and_then(|directory| directory.sync_all())
            .map_err(|error| Error::io("sync session metadata directory", error))?;
        Ok(())
    }

    fn owns(&self, metadata: &SessionMeta) -> bool {
        metadata
            .id
            .as_deref()
            .and_then(UserSessionId::try_parse)
            .as_ref()
            == Some(&self.shared.id)
            && metadata
                .working_directory
                .as_deref()
                .is_some_and(|path| self.owns_workspace(path))
    }

    fn owns_workspace(&self, working_directory: &str) -> bool {
        if working_directory == self.shared.launch_directory.as_os_str().to_string_lossy() {
            return true;
        }
        match (
            PathBuf::from(working_directory).canonicalize(),
            self.shared.launch_directory.canonicalize(),
        ) {
            (Ok(given), Ok(expected)) => given == expected,
            _ => false,
        }
    }
}

fn active_state(metadata: &SessionMeta) -> Result<SessionIdentityState> {
    let root_id = metadata
        .root_agent_session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| Error::invalid_data("root agent session identity is missing"))?;
    let root_name = metadata
        .root_agent_name
        .as_deref()
        .filter(|name| valid_name(name))
        .ok_or_else(|| Error::invalid_data("root agent name is missing or invalid"))?;
    let mode = metadata
        .mode
        .as_deref()
        .filter(|mode| !mode.trim().is_empty())
        .ok_or_else(|| Error::invalid_data("session mode is missing"))?;
    let revision = metadata
        .mode_revision
        .filter(|revision| *revision >= 0)
        .ok_or_else(|| Error::invalid_data("session mode revision is missing or invalid"))?;
    let records = metadata
        .agent_lineage
        .as_deref()
        .ok_or_else(|| Error::invalid_data("session lineage is missing"))?;
    let mut preceding = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if record.session_id.trim().is_empty()
            || !valid_name(&record.name)
            || preceding
                .insert(record.session_id.as_str(), index)
                .is_some()
            || (index == 0
                && (record.session_id != root_id
                    || record.name != root_name
                    || !record.parent_session_id.is_empty()))
            || (index > 0
                && (!preceding.contains_key(record.parent_session_id.as_str())
                    || record.parent_session_id == record.session_id))
        {
            return Err(Error::invalid_data(
                "session lineage has an invalid or conflicting identity",
            ));
        }
    }
    if records.is_empty() {
        return Err(Error::invalid_data(
            "session lineage does not contain the root identity",
        ));
    }
    Ok(SessionIdentityState {
        root_agent_session_id: root_id.into(),
        root_agent_name: root_name.into(),
        mode: PendingStatus {
            mode: mode.into(),
            version: revision,
        },
    })
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
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
