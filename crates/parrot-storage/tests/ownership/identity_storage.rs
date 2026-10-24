//! Original IdentityStorage assertions through real domain/Foundation/history APIs.
//! ProjectWorkspace/SecurityWriteTarget's source row is not covered here: no
//! corresponding accepted implementation is available in these dependencies.
use super::TempRoot;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::values::identifiers::{InvalidUserSessionId, UserSessionId};
use parrot_foundation::paths::{StatePaths, UserSessionResources};
use parrot_storage::conversation::{AgentHistoryEntry, ConversationPart, ToolCall};
use parrot_storage::history_files::{
    AgentHistoryFile, ErrorKind, SessionCatalog, SessionCatalogState, SessionIndex, SessionMeta,
};
use std::fs;
use std::io;
use std::path::PathBuf;

struct Fixture {
    _temp: TempRoot,
    paths: StatePaths,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempRoot::new("identity-storage");
        let workspace = temp.0.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let paths = StatePaths::new(
            temp.0.join("state"),
            temp.0.join("config"),
            temp.0.join("data"),
        );
        Self {
            _temp: temp,
            paths,
            workspace,
        }
    }
    fn resources(&self, name: &str) -> UserSessionResources<PathBuf> {
        let id = UserSessionId::parse(name).unwrap();
        let selected = SelectedSessionRoot::new(
            self.paths.state().join("sessions").join(id.value()),
            RootOrigin::Current,
        )
        .unwrap();
        UserSessionResources::new(selected, id, self.workspace.clone()).unwrap()
    }
    fn index(&self, resources: &UserSessionResources<PathBuf>) -> SessionIndex {
        SessionIndex::new(
            resources.selected_root().clone(),
            resources.id().clone(),
            &self.workspace,
        )
        .unwrap()
    }
    fn history(
        &self,
        resources: &UserSessionResources<PathBuf>,
        name: &[&str],
    ) -> AgentHistoryFile {
        let scratch = resources.agent_scratch(name).unwrap();
        AgentHistoryFile::new(
            resources.selected_root().clone(),
            scratch.history_path(),
            vec!["agent-session-child".into()],
        )
        .unwrap()
    }
}

#[test]
fn user_session_ids_reject_all_eight_exact_unsafe_source_arguments() {
    for value in [
        "",
        ".",
        "..",
        "../other",
        "other/session",
        "other\\session",
        "session\u{0001}",
        "/rooted",
    ] {
        assert!(
            UserSessionId::try_parse(value).is_none(),
            "TryParse {value:?}"
        );
        let _: InvalidUserSessionId = UserSessionId::parse(value).unwrap_err();
    }
}

#[test]
fn generated_and_safe_legacy_ids_are_valid() {
    let generated = UserSessionId::generate().unwrap();
    let legacy = UserSessionId::parse("legacy_session-17.alpha").unwrap();
    assert_eq!(UserSessionId::parse(generated.value()).unwrap(), generated);
    assert_eq!(legacy.value(), "legacy_session-17.alpha");
}

#[test]
fn session_resource_descriptors_are_contained_and_disjoint() {
    let fixture = Fixture::new();
    let first = fixture.resources("session-one");
    let second = fixture.resources("session-two");
    assert!(
        first
            .owns(&first.metadata_path(), &fixture.workspace)
            .unwrap()
    );
    let first_scratch = first.agent_scratch(&["agent-one"]).unwrap();
    let second_scratch = second.agent_scratch(&["agent-two"]).unwrap();
    assert!(
        first
            .owns(first_scratch.root(), &fixture.workspace)
            .unwrap()
    );
    assert!(
        first_scratch
            .contains(&first_scratch.blob_directory(), &fixture.workspace)
            .unwrap()
    );
    assert!(
        first_scratch
            .contains(&first_scratch.plan_directory(), &fixture.workspace)
            .unwrap()
    );
    assert!(
        !first_scratch
            .contains(second_scratch.root(), &fixture.workspace)
            .unwrap()
    );
    assert!(!first.owns(second.root(), &fixture.workspace).unwrap());
    assert!(!second.owns(first.root(), &fixture.workspace).unwrap());
    assert_eq!(
        first_scratch.root().parent().unwrap(),
        first.agents_directory()
    );
    assert_eq!(
        second_scratch.root().parent().unwrap(),
        second.agents_directory()
    );
}

#[test]
fn agent_scratch_immediately_provisions_its_root_and_blob_directory() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let expected_root = resources.agents_directory().join("main/worker/sub");
    let expected_blobs = expected_root.join("blobs");
    assert!(!expected_root.exists());
    assert!(!expected_blobs.exists());
    let scratch = resources.agent_scratch(&["main", "worker", "sub"]).unwrap();
    assert_eq!(scratch.root(), expected_root);
    assert_eq!(scratch.blob_directory(), expected_blobs);
    assert!(scratch.root().is_dir());
    assert!(scratch.blob_directory().is_dir());
}

#[test]
fn agent_queue_directories_are_contained_beneath_the_agents_queue_root() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let queue = resources
        .agent_queue_directory("agent-session-child")
        .unwrap();
    assert_eq!(
        resources.agent_queue_root_directory(),
        resources.queue_directory().join("agents")
    );
    assert_eq!(
        queue.parent().unwrap(),
        resources.agent_queue_root_directory()
    );
    assert_eq!(
        queue.strip_prefix(resources.queue_directory()).unwrap(),
        PathBuf::from("agents/agent-session-child")
    );
    assert!(
        resources
            .owns(&resources.agent_queue_root_directory(), &fixture.workspace)
            .unwrap()
    );
    assert!(resources.owns(&queue, &fixture.workspace).unwrap());
}

#[test]
fn agent_queue_directories_reject_all_ten_exact_unsafe_source_arguments() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    for value in [
        "",
        " ",
        ".",
        "..",
        "../other",
        "..\\other",
        "other/session",
        "other\\session",
        "/rooted",
        "\\rooted",
    ] {
        assert_eq!(
            resources.agent_queue_directory(value).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "queue argument {value:?}"
        );
    }
}

#[test]
fn agent_scratch_rejects_all_four_reserved_child_names_but_not_root_names() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    for reserved in ["blobs", "plan", "history.jsonl", "last_request.json"] {
        assert_eq!(
            resources
                .agent_scratch(&["main", reserved])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            resources
                .agent_scratch(&["main", "worker", reserved])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            resources.agent_scratch(&[]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            resources.agent_scratch(&[reserved]).unwrap().root(),
            resources.agents_directory().join(reserved)
        );
    }
}

#[test]
fn agent_history_files_are_isolated_under_the_session_agents_root() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let history = fixture.history(&resources, &["main", "child"]);
    assert_eq!(
        history.path(),
        resources
            .agents_directory()
            .join("main/child/history.jsonl")
    );
    assert!(resources.owns(history.path(), &fixture.workspace).unwrap());
    assert_eq!(
        history.path().strip_prefix(resources.root()).unwrap(),
        PathBuf::from("root-agents/main/child/history.jsonl")
    );
}

#[tokio::test]
async fn agent_history_projection_writes_discriminated_lowercase_json_lines() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let history = fixture.history(&resources, &["main"]);
    history
        .replace_entries(vec![AgentHistoryEntry::Compaction {
            sequence: 7,
            summary: "summary".into(),
            watermark: 4,
        }])
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(history.path()).unwrap(),
        "{\"type\":\"compaction\",\"summary\":\"summary\",\"watermark\":4,\"sequence\":7}\n"
    );
}

#[tokio::test]
async fn agent_history_projection_writes_structured_message_records() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let history = fixture.history(&resources, &["main"]);
    history
        .replace_entries(vec![AgentHistoryEntry::Message {
            sequence: 2,
            conversation_sequence: 9,
            origin: "model".into(),
            role: "assistant".into(),
            parts: vec![ConversationPart::Text("hello".into())],
            tool_calls: vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            tool_call_id: String::new(),
        }])
        .await
        .unwrap();
    let text = fs::read_to_string(history.path()).unwrap();
    assert!(text.contains("\"type\":\"message\""));
    assert!(text.contains("\"conversation_sequence\":9"));
    assert!(text.contains("\"arguments_json\":\"{}\""));
}

#[test]
fn agent_scratch_rejects_all_five_exact_unsafe_agent_names() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    for value in ["", "..", "../other", "other/session", "other\\session"] {
        assert_eq!(
            resources.agent_scratch(&[value]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            resources
                .agent_scratch(&["main", value])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn owner_bound_index_refuses_metadata_for_another_session_before_creating_directory() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-one");
    let index = fixture.index(&resources);
    let metadata = SessionMeta::new(
        "session-two",
        fixture.workspace.to_str().unwrap(),
        "provider",
        "provider/model",
    );
    assert_eq!(
        index.publish(&metadata).unwrap_err().kind,
        ErrorKind::Unowned
    );
    assert!(!resources.root().exists());
}

#[test]
fn catalog_isolates_corrupt_entries_and_omits_unsafe_directory_names() {
    let fixture = Fixture::new();
    let resources = fixture.resources("session-valid");
    fixture
        .index(&resources)
        .publish(&SessionMeta::new(
            resources.id().value(),
            fixture.workspace.to_str().unwrap(),
            "provider",
            "provider/model",
        ))
        .unwrap();
    let corrupt = fixture.paths.state().join("sessions/session-corrupt");
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("meta.json"), "not json").unwrap();
    fs::create_dir_all(fixture.paths.state().join("sessions/..unsafe")).unwrap();
    let listed = SessionCatalog::new(fixture.paths.state()).list();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed
            .iter()
            .find(|entry| entry.id.value() == "session-valid")
            .unwrap()
            .state,
        SessionCatalogState::Inactive
    );
    let invalid = listed
        .iter()
        .find(|entry| entry.id.value() == "session-corrupt")
        .unwrap();
    assert_eq!(invalid.state, SessionCatalogState::Corrupt);
    assert!(invalid.working_directory.is_empty());
    assert!(invalid.provider_id.is_empty());
    assert!(invalid.model.is_empty());
}
