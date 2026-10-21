use super::*;
use crate::conversation::{
    AgentHistoryEntry, ConversationMessage, ConversationRepository, MessageRole,
};
use crate::database::SessionDatabase;
use crate::events::EventRepository;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::protocol::messages::parrot::Event;
use parrot_domain::values::identifiers::UserSessionId;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t19-history-files-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn event(id: &str, agent: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: agent.into(),
        ..Default::default()
    }
}

fn repository(root: &SelectedSessionRoot) -> ConversationRepository {
    ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(root.clone()).unwrap(),
    ))
}

#[tokio::test]
async fn published_metadata_is_atomic_and_catalog_reads_metadata_without_a_database() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions").join("listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    let id = UserSessionId::try_parse("listed").unwrap();
    let root = SelectedSessionRoot::new(session_directory, RootOrigin::Current).unwrap();
    let index = SessionIndex::new(root, id, &launch_directory).unwrap();
    let metadata = SessionMeta::new(
        "listed",
        launch_directory.to_string_lossy(),
        "opencode-go",
        "deepseek-v4-pro",
    );

    index.publish(&metadata).unwrap();
    assert_eq!(index.find().unwrap(), Some(metadata.clone()));
    let catalog = SessionCatalog::new(&state);
    let listed = catalog.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.value(), "listed");
    assert!(
        std::fs::read_dir(index.selected_root().path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".staging"))
    );
    assert!(!index.selected_root().path().join("session.db").exists());

    let foreign = SessionMeta::new(
        "foreign",
        launch_directory.to_string_lossy(),
        "provider",
        "model",
    );
    assert_eq!(
        index.publish(&foreign).unwrap_err().kind,
        ErrorKind::Unowned
    );
    assert_eq!(catalog.list().len(), 1);
    assert_eq!(index.find().unwrap(), Some(metadata));
}

#[test]
fn metadata_directory_is_a_catalog_miss_and_remains_corrupt_in_catalog() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions/listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    let root = SelectedSessionRoot::new(session_directory.clone(), RootOrigin::Current).unwrap();
    let id = UserSessionId::try_parse("listed").unwrap();
    let index = SessionIndex::new(root, id.clone(), &launch_directory).unwrap();
    index
        .publish(&SessionMeta::new(
            "listed",
            launch_directory.to_string_lossy(),
            "provider",
            "model",
        ))
        .unwrap();

    std::fs::remove_file(index.metadata_path()).unwrap();
    std::fs::create_dir(index.metadata_path()).unwrap();

    assert_eq!(index.find().unwrap(), None);
    let catalog = SessionCatalog::new(&state);
    assert_eq!(
        catalog.find(&id).unwrap().state,
        SessionCatalogState::Corrupt
    );
}

#[test]
fn failed_metadata_staging_preserves_the_published_catalog_entry() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions/listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    let root = SelectedSessionRoot::new(session_directory, RootOrigin::Current).unwrap();
    let index = SessionIndex::new(
        root,
        UserSessionId::try_parse("listed").unwrap(),
        &launch_directory,
    )
    .unwrap();
    let mut original = SessionMeta::new(
        "listed",
        launch_directory.to_string_lossy(),
        "provider",
        "model",
    );
    original.root_agent_name = Some("main".into());
    index.publish(&original).unwrap();
    let original_bytes = std::fs::read(index.metadata_path()).unwrap();
    std::fs::create_dir(index.metadata_path().with_file_name("meta.json.staging")).unwrap();

    let mut replacement = original.clone();
    replacement.model = Some("model-after-failed-stage".into());
    assert!(index.publish(&replacement).is_err());
    assert_eq!(
        std::fs::read(index.metadata_path()).unwrap(),
        original_bytes
    );
    assert_eq!(
        SessionCatalog::new(&state)
            .find(&UserSessionId::try_parse("listed").unwrap())
            .unwrap()
            .model,
        "model"
    );
}

#[test]
fn metadata_replace_keeps_open_handles_on_the_previous_bytes() {
    use std::io::Read;

    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions/listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    let root = SelectedSessionRoot::new(session_directory, RootOrigin::Current).unwrap();
    let index = SessionIndex::new(
        root,
        UserSessionId::try_parse("listed").unwrap(),
        &launch_directory,
    )
    .unwrap();
    let original = SessionMeta::new(
        "listed",
        launch_directory.to_string_lossy(),
        "provider",
        "model",
    );
    index.publish(&original).unwrap();
    let old_bytes = std::fs::read(index.metadata_path()).unwrap();
    let mut old_handle = std::fs::File::open(index.metadata_path()).unwrap();
    let mut replacement = original;
    replacement.root_agent_name = Some("main".into());
    replacement.selector = Some("chosen".into());
    replacement.mode = Some("plan".into());
    index.publish(&replacement).unwrap();

    let mut observed_old = Vec::new();
    old_handle.read_to_end(&mut observed_old).unwrap();
    assert_eq!(observed_old, old_bytes);
    assert_ne!(std::fs::read(index.metadata_path()).unwrap(), old_bytes);
    assert_eq!(index.find().unwrap(), Some(replacement));
}

#[test]
fn explicit_null_metadata_preserves_index_and_catalog_source_outcomes() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions/listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    std::fs::create_dir_all(&session_directory).unwrap();
    let root = SelectedSessionRoot::new(session_directory.clone(), RootOrigin::Current).unwrap();
    let index = SessionIndex::new(
        root,
        UserSessionId::try_parse("listed").unwrap(),
        &launch_directory,
    )
    .unwrap();
    let catalog = SessionCatalog::new(&state);
    let baseline = serde_json::to_value(SessionMeta::new(
        "listed",
        launch_directory.to_string_lossy(),
        "provider",
        "model",
    ))
    .unwrap();
    let mut nullable = baseline.clone();
    for property in [
        "RootAgentName",
        "Selector",
        "Mode",
        "LastOpenedAt",
        "CreatedAt",
    ] {
        nullable[property] = serde_json::Value::Null;
    }
    std::fs::write(
        index.metadata_path(),
        serde_json::to_vec(&nullable).unwrap(),
    )
    .unwrap();
    let found = index.find().unwrap().unwrap();
    assert_eq!(found.root_agent_name, None);
    assert_eq!(found.selector, None);
    assert_eq!(found.mode, None);
    let listed = catalog
        .find(&UserSessionId::try_parse("listed").unwrap())
        .unwrap();
    assert_eq!(listed.state, SessionCatalogState::Inactive);
    assert_eq!(listed.root_agent_name, None);
    assert_eq!(listed.selector, None);
    assert_eq!(listed.mode, None);
    assert_eq!(listed.last_opened_at, None);
    assert_eq!(listed.created_at, None);

    index.publish(&found).unwrap();
    assert_eq!(index.find().unwrap(), Some(found));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(index.metadata_path()).unwrap())
            .unwrap(),
        nullable
    );

    // Index ownership is based on Id and WorkingDirectory, not provider/model.
    let mut nullable_provider = baseline.clone();
    nullable_provider["ProviderId"] = serde_json::Value::Null;
    nullable_provider["Model"] = serde_json::Value::Null;
    std::fs::write(
        index.metadata_path(),
        serde_json::to_vec(&nullable_provider).unwrap(),
    )
    .unwrap();
    let found = index.find().unwrap().unwrap();
    assert_eq!(found.provider_id, None);
    assert_eq!(found.model, None);
    assert_eq!(
        catalog
            .find(&UserSessionId::try_parse("listed").unwrap())
            .unwrap()
            .state,
        SessionCatalogState::Corrupt
    );

    for property in ["Id", "WorkingDirectory", "ProviderId", "Model"] {
        let mut contents = baseline.clone();
        contents[property] = serde_json::Value::Null;
        std::fs::write(
            index.metadata_path(),
            serde_json::to_vec(&contents).unwrap(),
        )
        .unwrap();
        let entry = catalog
            .find(&UserSessionId::try_parse("listed").unwrap())
            .unwrap();
        assert_eq!(entry.state, SessionCatalogState::Corrupt, "{property}");
        if property == "Id" || property == "WorkingDirectory" {
            assert!(index.find().unwrap().is_none(), "{property}");
        } else {
            assert!(index.find().unwrap().is_some(), "{property}");
        }
    }
}

#[test]
fn metadata_null_returns_no_index_hit_but_invalid_catalog_entry() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let sessions = state.join("sessions");
    std::fs::create_dir_all(&launch_directory).unwrap();
    let session_directory = sessions.join("listed");
    std::fs::create_dir_all(&session_directory).unwrap();
    std::fs::write(session_directory.join("meta.json"), "null").unwrap();
    let root = SelectedSessionRoot::new(session_directory, RootOrigin::Current).unwrap();
    let index = SessionIndex::new(
        root,
        UserSessionId::try_parse("listed").unwrap(),
        launch_directory,
    )
    .unwrap();

    assert_eq!(index.find().unwrap(), None);
    assert_eq!(
        SessionCatalog::new(state)
            .find(&UserSessionId::try_parse("listed").unwrap())
            .unwrap()
            .state,
        SessionCatalogState::Corrupt
    );
}

#[test]
fn snake_case_metadata_is_a_deserialization_error_not_a_catalog_hit() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let launch_directory = temp.0.join("workspace");
    let session_directory = state.join("sessions/listed");
    std::fs::create_dir_all(&launch_directory).unwrap();
    std::fs::create_dir_all(&session_directory).unwrap();
    std::fs::write(
        session_directory.join("meta.json"),
        r#"{"id":"listed","working_directory":"/workspace","provider_id":"provider","model":"model"}"#,
    )
    .unwrap();
    let root = SelectedSessionRoot::new(session_directory, RootOrigin::Current).unwrap();
    let index = SessionIndex::new(
        root,
        UserSessionId::try_parse("listed").unwrap(),
        launch_directory,
    )
    .unwrap();

    assert!(matches!(
        index.find(),
        Err(Error {
            kind: ErrorKind::InvalidData,
            ..
        })
    ));
    assert_eq!(
        SessionCatalog::new(state)
            .find(&UserSessionId::try_parse("listed").unwrap())
            .unwrap()
            .state,
        SessionCatalogState::Corrupt
    );
}

#[tokio::test]
async fn corrupt_metadata_is_isolated_from_valid_catalog_entries() {
    let temp = TempDirectory::new();
    let state = temp.0.join("state");
    let sessions = state.join("sessions");
    std::fs::create_dir_all(sessions.join("valid")).unwrap();
    std::fs::create_dir_all(sessions.join("corrupt")).unwrap();
    std::fs::write(
        sessions.join("valid").join("meta.json"),
        serde_json::to_vec(&SessionMeta::new(
            "valid",
            "/workspace",
            "provider",
            "model",
        ))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(sessions.join("corrupt").join("meta.json"), b"{").unwrap();

    let listed = SessionCatalog::new(state).list();
    assert_eq!(listed.len(), 2);
    assert!(
        listed.iter().any(
            |entry| entry.id.value() == "valid" && entry.state == SessionCatalogState::Inactive
        )
    );
    assert!(
        listed
            .iter()
            .any(|entry| entry.id.value() == "corrupt"
                && entry.state == SessionCatalogState::Corrupt)
    );
}

#[tokio::test]
async fn rebuild_preserves_all_occupants_then_refresh_appends_current_entries() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    repository
        .append_conversation(
            event("first", "first"),
            ConversationMessage::text(MessageRole::User, "first occupant"),
        )
        .await
        .unwrap();
    repository
        .append_conversation(
            event("second", "second"),
            ConversationMessage::text(MessageRole::User, "second occupant"),
        )
        .await
        .unwrap();

    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("root-agents/main/history.jsonl"),
            vec!["first".into(), "second".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("second".into(), projection)
        .unwrap();
    history.refresh().await.unwrap();
    let path = root.path().join("root-agents/main/history.jsonl");
    let first_text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = first_text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("first occupant"));
    assert!(lines[1].contains("second occupant"));

    history
        .append_conversation(
            event("later", "second"),
            ConversationMessage::text(MessageRole::User, "later current entry"),
        )
        .await
        .unwrap();
    let appended = std::fs::read_to_string(&path).unwrap();
    assert_eq!(appended.lines().count(), 3);
    assert!(
        appended
            .lines()
            .last()
            .unwrap()
            .contains("later current entry")
    );
    repository.events().database().close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_reprovisions_agent_directories_before_later_append() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    repository
        .append_conversation(
            event("before-recovery", "current"),
            ConversationMessage::text(MessageRole::User, "before recovery"),
        )
        .await
        .unwrap();
    let parent = root.path().join("root-agents/main");
    let path = parent.join("history.jsonl");
    let projection = Arc::new(
        AgentHistoryFile::new(root.clone(), path.clone(), vec!["current".into()]).unwrap(),
    );
    let history = repository
        .bind_agent_history("current".into(), projection)
        .unwrap();
    history.refresh().await.unwrap();
    let original = std::fs::read(&path).unwrap();
    assert!(original.ends_with(b"\n"));
    std::fs::remove_dir_all(&parent).unwrap();
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    for directory in [&parent, &parent.join("scratch"), &parent.join("blobs")] {
        assert!(std::fs::symlink_metadata(directory).unwrap().is_dir());
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    history
        .append_conversation(
            event("after-recovery", "current"),
            ConversationMessage::text(MessageRole::User, "after recovery"),
        )
        .await
        .unwrap();
    let appended = std::fs::read(&path).unwrap();
    assert!(appended.starts_with(&original));
    assert!(appended.ends_with(b"\n"));
    assert_eq!(
        String::from_utf8(appended.clone()).unwrap().lines().count(),
        2
    );
    let mut expected = Vec::new();
    for entry in repository.agent_history("current".into()).await.unwrap() {
        expected.extend(json::serialize_entry(&entry).unwrap());
        expected.push(b'\n');
    }
    assert_eq!(appended, expected);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(std::fs::read_dir(&parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), appended);
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn explicit_replace_entries_matches_source_atomic_failure_behavior() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let projection = AgentHistoryFile::new(
        root.clone(),
        root.path().join("agent/history.jsonl"),
        vec!["agent".into()],
    )
    .unwrap();
    let initial = AgentHistoryEntry::Compaction {
        sequence: 7,
        summary: "summary".into(),
        watermark: 4,
    };
    projection.replace_entries(vec![initial]).await.unwrap();
    let original = std::fs::read(projection.path()).unwrap();
    assert!(
        std::fs::read_dir(projection.path().parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
    );

    let invalid = AgentHistoryEntry::Request {
        sequence: 8,
        event_sequence: 9,
        request_id: "invalid-cost".into(),
        provider: "provider".into(),
        model: "model".into(),
        effort: None,
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        input_cost: f64::NAN,
        output_cost: 0.0,
        tool_calls: 0,
    };
    assert_eq!(
        projection
            .replace_entries(vec![invalid])
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    assert_eq!(std::fs::read(projection.path()).unwrap(), original);
    assert!(
        std::fs::read_dir(projection.path().parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
    );
}

#[tokio::test]
async fn refresh_rebuilds_truncated_deleted_changed_and_directory_targets() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    repository
        .append_conversation(
            event("durable", "agent"),
            ConversationMessage::text(MessageRole::User, "database authority"),
        )
        .await
        .unwrap();
    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    history.refresh().await.unwrap();
    let expected = std::fs::read(projection.path()).unwrap();

    std::fs::write(projection.path(), &expected[..expected.len() / 2]).unwrap();
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), expected);
    std::fs::remove_file(projection.path()).unwrap();
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), expected);
    std::fs::write(projection.path(), b"external edit\n").unwrap();
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), expected);
    std::fs::remove_file(projection.path()).unwrap();
    std::fs::create_dir(projection.path()).unwrap();
    std::fs::write(projection.path().join("unexpected"), b"not authoritative").unwrap();
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), expected);
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn same_length_external_edit_is_not_rebuilt_without_new_history() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    repository
        .append_conversation(
            event("durable", "agent"),
            ConversationMessage::text(MessageRole::User, "current occupant"),
        )
        .await
        .unwrap();
    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    history.refresh().await.unwrap();

    let expected = std::fs::read_to_string(projection.path()).unwrap();
    let changed = expected.replace("current occupant", "changed occupant");
    assert_eq!(changed.len(), expected.len());
    std::fs::write(projection.path(), &changed).unwrap();
    history.refresh().await.unwrap();

    assert_eq!(std::fs::read_to_string(projection.path()).unwrap(), changed);
    repository.events().database().close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn refresh_replaces_symlinks_without_writing_their_targets() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    history
        .append_conversation(
            event("first", "agent"),
            ConversationMessage::text(MessageRole::User, "first"),
        )
        .await
        .unwrap();
    let expected = std::fs::read(projection.path()).unwrap();
    let target = root.path().join("external");
    std::fs::write(&target, &expected).unwrap();
    std::fs::remove_file(projection.path()).unwrap();
    symlink(&target, projection.path()).unwrap();
    history
        .append_conversation(
            event("second", "agent"),
            ConversationMessage::text(MessageRole::User, "second"),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), expected);
    let metadata = std::fs::symlink_metadata(projection.path()).unwrap();
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(projection.path())
            .unwrap()
            .lines()
            .count(),
        2
    );
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn known_good_refresh_appends_without_replacing_the_file() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("agent/history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("agent".into(), projection.clone())
        .unwrap();
    history
        .append_conversation(
            event("first", "agent"),
            ConversationMessage::text(MessageRole::User, "first"),
        )
        .await
        .unwrap();
    let original = std::fs::read(projection.path()).unwrap();
    // An open handle observes append, but would keep the old bytes after replace.
    let mut handle = std::fs::File::open(projection.path()).unwrap();
    history
        .append_conversation(
            event("second", "agent"),
            ConversationMessage::text(MessageRole::User, "second"),
        )
        .await
        .unwrap();
    let appended = std::fs::read(projection.path()).unwrap();
    assert!(appended.starts_with(&original));
    let mut observed = Vec::new();
    std::io::Read::read_to_end(&mut handle, &mut observed).unwrap();
    assert_eq!(observed, appended);
    history.refresh().await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), appended);
    history.cleanup_fork().await.unwrap();
    assert!(std::fs::read(projection.path()).unwrap().is_empty());
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn projection_rejects_the_wrong_session_even_when_repository_is_closed() {
    use crate::conversation::HistoryProjection;

    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    let projection = AgentHistoryFile::new(
        root.clone(),
        root.path().join("agent/history.jsonl"),
        vec!["agent".into()],
    )
    .unwrap();
    projection.refresh(&repository, "agent").await.unwrap();
    let expected = std::fs::read(projection.path()).unwrap();
    repository.events().database().close().await.unwrap();
    projection.refresh(&repository, "agent").await.unwrap();
    projection.rebuild(&repository, "agent").await.unwrap();
    assert_eq!(std::fs::read(projection.path()).unwrap(), expected);
    assert!(matches!(
        projection.refresh(&repository, "other").await,
        Err(crate::conversation::EventError::Invalid(message))
            if message == "History projection belongs to another agent session."
    ));
    assert!(matches!(
        projection.rebuild(&repository, "other").await,
        Err(crate::conversation::EventError::Invalid(message))
            if message == "History projection belongs to another agent session."
    ));
}

#[tokio::test]
async fn concurrent_agent_history_updates_remain_isolated() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let repository = repository(&root);
    let first_projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("root-agents/main/history.jsonl"),
            vec!["first".into()],
        )
        .unwrap(),
    );
    let second_projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            root.path().join("root-agents/second/history.jsonl"),
            vec!["second".into()],
        )
        .unwrap(),
    );
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..12 {
        for (agent, projection) in [
            ("first", Arc::clone(&first_projection)),
            ("second", Arc::clone(&second_projection)),
        ] {
            let repository = repository.clone();
            tasks.spawn(async move {
                repository
                    .bind_agent_history(agent.into(), projection)
                    .unwrap()
                    .append_conversation(
                        event(&format!("{agent}-{index}"), agent),
                        ConversationMessage::text(MessageRole::User, format!("{agent}-{index}")),
                    )
                    .await
                    .unwrap();
            });
        }
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }

    let first = std::fs::read_to_string(first_projection.path()).unwrap();
    let second = std::fs::read_to_string(second_projection.path()).unwrap();
    assert_eq!(first.lines().count(), 12);
    assert!(first.lines().all(|line| line.contains("first-")));
    assert_eq!(second.lines().count(), 12);
    assert!(second.lines().all(|line| line.contains("second-")));
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn history_file_failure_does_not_undo_committed_conversation() {
    let temp = TempDirectory::new();
    let root = SelectedSessionRoot::new(temp.0.clone(), RootOrigin::Current).unwrap();
    let blocked_parent = root.path().join("blocked");
    std::fs::write(&blocked_parent, "not a directory").unwrap();
    let repository = repository(&root);
    let projection = Arc::new(
        AgentHistoryFile::new(
            root.clone(),
            blocked_parent.join("history.jsonl"),
            vec!["agent".into()],
        )
        .unwrap(),
    );
    let history = repository
        .bind_agent_history("agent".into(), projection)
        .unwrap();

    history
        .append_conversation(
            event("durable", "agent"),
            ConversationMessage::text(MessageRole::User, "committed despite file failure"),
        )
        .await
        .unwrap();
    assert_eq!(
        repository.messages("agent".into()).await.unwrap(),
        ["user: committed despite file failure"]
    );
    assert_eq!(repository.events().replay().await.unwrap().len(), 1);
    assert!(!blocked_parent.join("history.jsonl").exists());
    std::fs::remove_file(&blocked_parent).unwrap();
    history.refresh().await.unwrap();
    assert!(
        std::fs::read_to_string(blocked_parent.join("history.jsonl"))
            .unwrap()
            .contains("committed despite file failure")
    );
    assert!(std::fs::read_dir(&blocked_parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
    repository.events().database().close().await.unwrap();
}
