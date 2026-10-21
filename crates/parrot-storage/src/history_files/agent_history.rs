use super::json::serialize_entry;
use crate::conversation::{
    AgentHistoryEntry, ConversationRepository, EventError, HistoryProjection,
};
use crate::database::DatabaseError;
use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_domain::contracts::paths::SelectedSessionRoot;
use parrot_domain::values::identifiers::Identifier;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// A recoverable JSONL projection bound to one agent session and selected root.
/// SQLite remains authoritative; this file is only rewritten from repository history.
pub struct AgentHistoryFile {
    root: SelectedSessionRoot,
    path: PathBuf,
    occupant_session_ids: Vec<String>,
    agent_session_id: String,
    state: Arc<Mutex<ProjectionState>>,
}

#[derive(Default)]
struct ProjectionState {
    history_sequence: i64,
    request_event_sequence: i64,
    written_length: Option<u64>,
}

impl AgentHistoryFile {
    /// Bind this projection to a concrete history path beneath the selected root.
    pub fn new(
        root: SelectedSessionRoot,
        path: impl Into<PathBuf>,
        occupant_session_ids: Vec<String>,
    ) -> Result<Self, super::Error> {
        let path = path.into();
        if !path.starts_with(root.path()) {
            return Err(super::Error::invalid_input(
                "history projection path must be beneath the selected session root",
            ));
        }
        let Some(agent_session_id) = occupant_session_ids.last().cloned() else {
            return Err(super::Error::invalid_input(
                "history projection requires at least one occupant session id",
            ));
        };
        if occupant_session_ids.iter().any(|id| id.trim().is_empty()) {
            return Err(super::Error::invalid_input(
                "agent session id must not be empty or whitespace",
            ));
        }
        Ok(Self {
            root,
            path,
            occupant_session_ids,
            agent_session_id: agent_session_id.clone(),
            state: Arc::new(Mutex::new(ProjectionState::default())),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Explicitly replace the JSONL projection without best-effort suppression.
    pub async fn replace_entries(&self, entries: Vec<AgentHistoryEntry>) -> super::Result<()> {
        let state = self.state.clone().lock_owned().await;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state;
            state.written_length = None;
            replace_entries(&path, &entries)
        })
        .await
        .map_err(|error| {
            super::Error::new(
                super::ErrorKind::Io,
                format!("history replacement worker failed: {error}"),
            )
        })?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                super::Error::invalid_data(format!("replace agent history file: {error}"))
            } else if error.kind() == std::io::ErrorKind::InvalidInput {
                super::Error::new(super::ErrorKind::InvalidOperation, error.to_string())
            } else {
                super::Error::io("replace agent history file", error)
            }
        })?;
        Ok(())
    }

    fn validate(&self, agent: &str) -> Result<(), EventError> {
        if agent != self.agent_session_id {
            return Err(EventError::Invalid(
                "History projection belongs to another agent session.".into(),
            ));
        }
        Ok(())
    }

    async fn refresh_inner(&self, repository: &ConversationRepository) -> Result<(), EventError> {
        let state = self.state.clone().lock_owned().await;
        let path = self.path.clone();
        // Blocking workers own the gate, including after the awaiting future is dropped.
        let (is_written, state) = tokio::task::spawn_blocking(move || {
            let written = is_written(&path, state.written_length);
            (written, state)
        })
        .await
        .map_err(|error| {
            EventError::Invalid(format!("history inspection worker failed: {error}"))
        })?;
        if !is_written {
            return self.rebuild_locked(repository, state).await;
        }
        let entries = repository
            .agent_history_after(
                self.agent_session_id.clone(),
                state.history_sequence,
                state.request_event_sequence,
            )
            .await?;
        if entries.is_empty() {
            return Ok(());
        }
        let path = self.path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let mut state = state;
            state.written_length = None;
            if let Ok(written_length) = append_entries(&path, &entries) {
                advance(&mut state, &entries);
                state.written_length = Some(written_length);
            }
        })
        .await;
        Ok(())
    }

    async fn rebuild_inner(&self, repository: &ConversationRepository) -> Result<(), EventError> {
        let state = self.state.clone().lock_owned().await;
        self.rebuild_locked(repository, state).await
    }

    async fn rebuild_locked(
        &self,
        repository: &ConversationRepository,
        mut state: OwnedMutexGuard<ProjectionState>,
    ) -> Result<(), EventError> {
        state.written_length = None;
        state.history_sequence = 0;
        state.request_event_sequence = 0;
        let mut entries = Vec::new();
        let mut current_entries = Vec::new();
        for (index, occupant) in self.occupant_session_ids.iter().enumerate() {
            let occupant_entries = repository.agent_history(occupant.clone()).await?;
            if index + 1 == self.occupant_session_ids.len() {
                current_entries.clone_from(&occupant_entries);
            }
            entries.extend(occupant_entries);
        }
        let path = self.path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(written_length) = replace_entries(&path, &entries) {
                advance(&mut state, &current_entries);
                state.written_length = Some(written_length);
            }
        })
        .await;
        Ok(())
    }
}

impl HistoryProjection for AgentHistoryFile {
    fn selected_root(&self) -> &SelectedSessionRoot {
        &self.root
    }

    fn validate_session(&self, agent: &str) -> Result<(), EventError> {
        self.validate(agent)
    }

    fn refresh<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            self.validate(agent)?;
            best_effort(self.refresh_inner(repository).await)
        })
    }

    fn rebuild<'a>(
        &'a self,
        repository: &'a ConversationRepository,
        agent: &'a str,
    ) -> BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async move {
            self.validate(agent)?;
            best_effort(self.rebuild_inner(repository).await)
        })
    }
}

// Source catches IO/unauthorized-access and invalid-operation failures only.
// Its InvalidProtocolBufferException derives from IOException, so malformed
// request-event decoding is best effort here, but not on direct repository reads.
// Keep argument validation outside this boundary and SQLite/worker failures visible.
fn best_effort(result: Result<(), EventError>) -> Result<(), EventError> {
    match result {
        Err(EventError::Invalid(_) | EventError::Conflict(_))
        | Err(EventError::Database(
            DatabaseError::Io(_) | DatabaseError::Closed | DatabaseError::Protobuf(_),
        )) => Ok(()),
        result => result,
    }
}

fn is_written(path: &Path, written_length: Option<u64>) -> bool {
    let Some(written_length) = written_length else {
        return false;
    };
    if validate_parent(path).is_err() {
        return false;
    }
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file() && metadata.len() == written_length)
        .unwrap_or(false)
}

fn append_entries(path: &Path, entries: &[AgentHistoryEntry]) -> std::io::Result<u64> {
    validate_parent(path)?;
    let mut file = OpenOptions::new().append(true).open(path)?;
    for entry in entries {
        file.write_all(&serialize_entry(entry).map_err(invalid_data)?)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

// Match source scratch-root validation, not full ancestor confinement or race protection.
fn validate_parent(path: &Path) -> std::io::Result<&Path> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "history path has no parent",
        )
    })?;
    validate_directory(parent)?;
    Ok(parent)
}

fn validate_directory(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "agent scratch directory cannot be a symbolic link",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "agent scratch path is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn replace_entries(path: &Path, entries: &[AgentHistoryEntry]) -> std::io::Result<u64> {
    let parent = validate_parent(path)?;
    for directory in [
        parent.to_path_buf(),
        parent.join("scratch"),
        parent.join("blobs"),
    ] {
        validate_directory(&directory)?;
        fs::create_dir_all(&directory)?;
        validate_directory(&directory)?;
        set_directory_mode(&directory)?;
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(path)?;
        } else if metadata.file_type().is_symlink() {
            fs::remove_file(path)?;
        }
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("history.jsonl");
    let temp = parent.join(format!(".{name}.{}.tmp", Identifier::event_id()?));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        for entry in entries {
            file.write_all(&serialize_entry(entry).map_err(invalid_data)?)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        set_file_mode(path)?;
        Ok(fs::metadata(path)?.len())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn invalid_data(error: super::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

fn advance(state: &mut ProjectionState, entries: &[AgentHistoryEntry]) {
    for entry in entries {
        match entry {
            AgentHistoryEntry::Request { event_sequence, .. } => {
                state.request_event_sequence = state.request_event_sequence.max(*event_sequence);
            }
            _ => state.history_sequence = state.history_sequence.max(entry.sequence()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn replacement_publishes_jsonl_with_a_trailing_newline() {
        let directory = std::env::temp_dir().join(format!(
            "parrot-history-files-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("history.jsonl");
        let entries = [AgentHistoryEntry::Compaction {
            sequence: 4,
            summary: "saved summary".into(),
            watermark: 3,
        }];

        replace_entries(&path, &entries).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"type\":\"compaction\",\"summary\":\"saved summary\",\"watermark\":3,\"sequence\":4}\n"
        );
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dropped_replace_future_keeps_worker_serialization_until_disk_work_finishes() {
        use parrot_domain::contracts::paths::RootOrigin;
        use std::future::Future;
        use std::task::Poll;

        let directory = std::env::temp_dir().join(format!(
            "parrot-history-drop-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let projection = AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["current".into()],
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            started_rx.await.unwrap();
            let entry = |summary: &str| {
                vec![AgentHistoryEntry::Compaction {
                    sequence: 1,
                    summary: summary.into(),
                    watermark: 0,
                }]
            };
            let mut first = Box::pin(projection.replace_entries(entry("older")));
            std::future::poll_fn(|context| {
                assert!(first.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(first);
            // Its queued blocking worker, not the dropped future, owns the gate.
            assert!(projection.state.try_lock().is_err());
            let mut second = Box::pin(projection.replace_entries(entry("newer")));
            std::future::poll_fn(|context| {
                assert!(second.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            second.await.unwrap();
            assert_eq!(
                fs::read_to_string(projection.path()).unwrap(),
                "{\"type\":\"compaction\",\"summary\":\"newer\",\"watermark\":0,\"sequence\":1}\n"
            );
        });
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_scratch_parent_is_rejected_without_touching_referent() {
        use parrot_domain::contracts::paths::RootOrigin;
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = std::env::temp_dir().join(format!(
            "parrot-history-parent-link-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let target = directory.join("external");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let target_history = target.join("history.jsonl");
        fs::write(&target_history, b"unchanged").unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let parent = root.path().join("agent");
        symlink(&target, &parent).unwrap();
        let projection = AgentHistoryFile::new(
            root.clone(),
            parent.join("history.jsonl"),
            vec!["current".into()],
        )
        .unwrap();
        assert!(projection.replace_entries(Vec::new()).await.is_err());
        assert!(!is_written(projection.path(), Some(9)));
        assert!(append_entries(projection.path(), &[]).is_err());
        let repository = ConversationRepository::new(crate::events::EventRepository::new(
            crate::database::SessionDatabase::open(root).unwrap(),
        ));
        projection.refresh(&repository, "current").await.unwrap();
        projection.rebuild(&repository, "current").await.unwrap();
        assert_eq!(fs::read(&target_history).unwrap(), b"unchanged");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        repository.events().database().close().await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn wrong_session_validation_precedes_closed_repository_and_preserves_history() {
        use parrot_domain::contracts::paths::RootOrigin;

        let directory = std::env::temp_dir().join(format!(
            "parrot-history-wrong-session-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let repository = ConversationRepository::new(crate::events::EventRepository::new(
            crate::database::SessionDatabase::open(root.clone()).unwrap(),
        ));
        let projection = AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["current".into()],
        )
        .unwrap();
        projection
            .replace_entries(vec![AgentHistoryEntry::Compaction {
                sequence: 1,
                summary: "unchanged".into(),
                watermark: 0,
            }])
            .await
            .unwrap();
        let expected = fs::read(projection.path()).unwrap();
        repository.events().database().close().await.unwrap();

        for result in [
            projection.validate_session("other"),
            projection.refresh(&repository, "other").await,
            projection.rebuild(&repository, "other").await,
        ] {
            assert!(matches!(
                result,
                Err(EventError::Invalid(message))
                    if message == "History projection belongs to another agent session."
            ));
            assert_eq!(fs::read(projection.path()).unwrap(), expected);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn best_effort_only_suppresses_source_equivalent_failures() {
        assert!(best_effort(Err(EventError::Invalid("invalid history state".into()))).is_ok());
        assert!(best_effort(Err(EventError::Conflict("invalid operation".into()))).is_ok());
        assert!(
            best_effort(Err(EventError::Database(DatabaseError::Io(
                std::io::ErrorKind::PermissionDenied.into(),
            ))))
            .is_ok()
        );
        assert!(best_effort(Err(EventError::Database(DatabaseError::Closed))).is_ok());
        assert!(best_effort(Err(EventError::Argument("wrong agent".into()))).is_err());
        assert!(best_effort(Err(EventError::Overflow)).is_err());
        assert!(
            best_effort(Err(EventError::Database(DatabaseError::Sqlite(
                rusqlite::Error::InvalidQuery,
            ))))
            .is_err()
        );
        assert!(best_effort(Err(EventError::Database(DatabaseError::CommandPanicked))).is_err());
    }

    #[tokio::test]
    async fn rebuild_cursors_only_include_current_occupant_and_reset_before_read_failure() {
        use crate::conversation::{ConversationMessage, MessageRole};
        use crate::database::SessionDatabase;
        use crate::events::EventRepository;
        use parrot_domain::contracts::paths::RootOrigin;
        use parrot_domain::protocol::messages::parrot::{Event, RequestUsageRecorded};

        let directory = std::env::temp_dir().join(format!(
            "parrot-history-cursors-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let repository = ConversationRepository::new(EventRepository::new(
            SessionDatabase::open(root.clone()).unwrap(),
        ));
        // The previous occupant has later global history/event sequence numbers.
        for agent in ["current", "previous"] {
            repository
                .append_conversation(
                    Event {
                        id: format!("message-{agent}"),
                        agent_session_id: agent.into(),
                        ..Default::default()
                    },
                    ConversationMessage::text(MessageRole::User, agent),
                )
                .await
                .unwrap();
            let mut request = Event {
                id: format!("request-event-{agent}"),
                agent_session_id: agent.into(),
                ..Default::default()
            };
            request.set_request_usage_recorded(RequestUsageRecorded {
                request_id: format!("request-{agent}"),
                provider: "provider".into(),
                model: "model".into(),
                ..Default::default()
            });
            repository
                .events()
                .append_usage_fact(request)
                .await
                .unwrap();
        }
        let projection = AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["previous".into(), "current".into()],
        )
        .unwrap();
        projection.rebuild(&repository, "current").await.unwrap();
        let current = repository.agent_history("current".into()).await.unwrap();
        let mut expected = ProjectionState::default();
        advance(&mut expected, &current);
        {
            let state = projection.state.lock().await;
            assert_eq!(state.history_sequence, expected.history_sequence);
            assert_eq!(
                state.request_event_sequence,
                expected.request_event_sequence
            );
            assert!(state.written_length.is_some());
        }
        let original = fs::read(projection.path()).unwrap();
        let mut request = Event {
            id: "later-current-request-event".into(),
            agent_session_id: "current".into(),
            ..Default::default()
        };
        request.set_request_usage_recorded(RequestUsageRecorded {
            request_id: "later-current-request".into(),
            provider: "provider".into(),
            model: "model".into(),
            ..Default::default()
        });
        repository
            .events()
            .append_usage_fact(request)
            .await
            .unwrap();
        projection.refresh(&repository, "current").await.unwrap();
        let appended = fs::read(projection.path()).unwrap();
        assert!(appended.starts_with(&original));
        let text = String::from_utf8(appended.clone()).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(
            text.lines()
                .last()
                .unwrap()
                .contains("later-current-request")
        );
        projection.refresh(&repository, "current").await.unwrap();
        assert_eq!(fs::read(projection.path()).unwrap(), appended);
        repository.events().database().close().await.unwrap();
        projection.rebuild(&repository, "current").await.unwrap();
        {
            let state = projection.state.lock().await;
            assert_eq!(state.history_sequence, 0);
            assert_eq!(state.request_event_sequence, 0);
            assert!(state.written_length.is_none());
        }
        assert_eq!(fs::read(projection.path()).unwrap(), appended);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_obstructions_preserve_target_and_prior_directory_effects() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for child in ["scratch", "blobs"] {
            for link in [false, true] {
                for directory_target in [false, true] {
                    let directory = std::env::temp_dir().join(format!(
                        "parrot-history-provision-{}-{}",
                        std::process::id(),
                        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
                    ));
                    let parent = directory.join("agent");
                    let external = directory.join("external");
                    fs::create_dir_all(&parent).unwrap();
                    fs::create_dir_all(&external).unwrap();
                    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
                    fs::set_permissions(&external, fs::Permissions::from_mode(0o755)).unwrap();
                    fs::write(external.join("sentinel"), b"external unchanged\n").unwrap();
                    if child == "blobs" {
                        fs::create_dir(parent.join("scratch")).unwrap();
                        fs::set_permissions(
                            parent.join("scratch"),
                            fs::Permissions::from_mode(0o755),
                        )
                        .unwrap();
                    }
                    let obstruction = parent.join(child);
                    if link {
                        symlink(&external, &obstruction).unwrap();
                    } else {
                        fs::write(&obstruction, b"obstruction unchanged\n").unwrap();
                    }
                    let path = parent.join("history.jsonl");
                    let sentinel = if directory_target {
                        fs::create_dir(&path).unwrap();
                        path.join("sentinel")
                    } else {
                        path.clone()
                    };
                    fs::write(&sentinel, b"history unchanged\n").unwrap();

                    let error = replace_entries(&path, &[]).unwrap_err();
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
                    assert_eq!(fs::read(&sentinel).unwrap(), b"history unchanged\n");
                    assert_eq!(
                        fs::symlink_metadata(&path).unwrap().is_dir(),
                        directory_target
                    );
                    assert_eq!(
                        fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
                        0o700
                    );
                    if child == "blobs" {
                        assert_eq!(
                            fs::metadata(parent.join("scratch"))
                                .unwrap()
                                .permissions()
                                .mode()
                                & 0o777,
                            0o700
                        );
                    } else {
                        assert!(!parent.join("blobs").exists());
                    }
                    if link {
                        assert_eq!(fs::read_link(&obstruction).unwrap(), external);
                    } else {
                        assert_eq!(fs::read(&obstruction).unwrap(), b"obstruction unchanged\n");
                    }
                    assert_eq!(
                        fs::metadata(&external).unwrap().permissions().mode() & 0o777,
                        0o755
                    );
                    assert_eq!(
                        fs::read(external.join("sentinel")).unwrap(),
                        b"external unchanged\n"
                    );
                    assert_eq!(fs::read_dir(&external).unwrap().count(), 1);
                    assert!(fs::read_dir(&parent).unwrap().all(|entry| {
                        !entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .ends_with(".tmp")
                    }));
                    fs::remove_dir_all(directory).unwrap();
                }
            }
        }
    }

    #[test]
    fn non_directory_root_is_rejected_without_replacing_it() {
        let directory = std::env::temp_dir().join(format!(
            "parrot-history-root-file-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let parent = directory.join("agent");
        fs::write(&parent, b"root unchanged\n").unwrap();
        assert_eq!(
            replace_entries(&parent.join("history.jsonl"), &[])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(&parent).unwrap(), b"root unchanged\n");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changed_file_invalidates_the_last_written_length() {
        let directory = std::env::temp_dir().join(format!(
            "parrot-history-files-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("history.jsonl");
        replace_entries(&path, &[]).unwrap();
        let written_length = fs::metadata(&path).unwrap().len();
        assert!(is_written(&path, Some(written_length)));
        fs::write(&path, "external change\n").unwrap();
        assert!(!is_written(&path, Some(written_length)));
        fs::remove_dir_all(directory).unwrap();
    }
}
