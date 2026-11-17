use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use parrot_core::NeverCancelled;
use parrot_domain::{
    contracts::{
        paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
        security::{AccessSnapshot, InvocationScope},
        tools::Tool,
    },
    protocol::messages::parrot::ClipboardWrite,
    values::identifiers::UserSessionId,
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::event_broker::EventBroker;
use parrot_storage::{
    conversation::{AgentHistoryView, ConversationRepository},
    database::SessionDatabase,
    events::EventRepository,
    history_files::{AgentHistoryFile, SessionIndex, SessionMeta},
};
use parrot_tools::{
    clipboard::{CLIPBOARD_NOTICE_KIND, ClipboardCopyTool, Invocation},
    session::Failure,
};
use protobuf::Message;

#[derive(Clone)]
pub struct Security;
impl AccessSnapshot for Security {
    type Error = Failure;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        Ok(())
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
fn id() -> String {
    format!("clipboard-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn templates() -> Arc<PromptTemplateCatalog> {
    let env = [
        ("HOME", "/tmp/clipboard/home"),
        ("XDG_CONFIG_HOME", "/tmp/clipboard/config"),
        ("XDG_STATE_HOME", "/tmp/clipboard/state"),
        ("XDG_DATA_HOME", "/tmp/clipboard/data"),
        ("WORKDIR", "/tmp/clipboard/work"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    Arc::new(PromptTemplateCatalog::from_content("", &env).unwrap().1)
}

pub struct Fixture {
    history: AgentHistoryView,
    publisher: Arc<EventBroker>,
    templates: Arc<PromptTemplateCatalog>,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("retained-{}", id()));
        std::fs::create_dir_all(directory.join("scratch")).unwrap();
        let root = SelectedSessionRoot::new(directory.clone(), RootOrigin::Current).unwrap();
        let root_session = root.path().file_name().unwrap().to_str().unwrap();
        let launch_directory = std::env::current_dir().unwrap();
        let metadata = SessionIndex::new(
            root.clone(),
            UserSessionId::parse(root_session).unwrap(),
            launch_directory.clone(),
        )
        .unwrap();
        if metadata.find().unwrap().is_none() {
            let mut session = SessionMeta::new(
                root_session,
                launch_directory.to_str().unwrap(),
                "provider",
                "model",
            );
            session.root_agent_name = Some("main".into());
            session.mode = Some("initial".into());
            metadata.initialize(session, "fixture-root".into()).unwrap();
        }
        let repository = ConversationRepository::new(
            EventRepository::new(SessionDatabase::open(root.clone()).unwrap()),
            metadata,
        )
        .unwrap();
        let agent = id();
        let history = repository
            .bind_agent_history(
                agent.clone(),
                Arc::new(
                    AgentHistoryFile::new(
                        root.clone(),
                        directory.join("history.jsonl"),
                        vec![agent.clone()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        Self {
            history,
            publisher: Arc::new(EventBroker::new()),
            templates: templates(),
        }
    }

    pub fn tool(&self) -> ClipboardCopyTool<Security, EventBroker> {
        ClipboardCopyTool::new(definition(), self.history.clone(), self.publisher.clone())
    }

    pub fn invocation(&self, text: &str) -> Invocation {
        Invocation {
            call_id: format!("{}/0", id()),
            arguments_json: serde_json::json!({ "text": text }).to_string(),
            prompt_templates: self.templates.clone(),
        }
    }

    pub async fn notices(&self) -> Vec<ClipboardWrite> {
        self.history
            .notices(CLIPBOARD_NOTICE_KIND.to_owned())
            .await
            .unwrap()
            .into_iter()
            .map(|notice| ClipboardWrite::parse_from_bytes(&notice.data).unwrap())
            .collect()
    }
}

pub fn definition() -> parrot_domain::values::tools::ConfiguredToolDefinition {
    parrot_domain::values::tools::ConfiguredToolDefinition::new(
        "Copy text to the user's clipboard with the terminal-native OSC 52 sequence.",
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
    )
}

#[tokio::test]
async fn copies_text_and_appends_exactly_one_notice_with_the_encoded_text() {
    let fixture = Fixture::new();
    let tool = fixture.tool();
    let output = tool
        .invoke(
            fixture.invocation("Hello from parrot"),
            InvocationScope::new(Security, (), &NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(output, "Copied 17 bytes to the clipboard.");
    let notices = fixture.notices().await;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].base64_text, STANDARD.encode("Hello from parrot"));
}

#[tokio::test]
async fn text_over_the_cap_fails_without_appending_a_notice() {
    let fixture = Fixture::new();
    let tool = fixture.tool();
    let oversized: String = "x".repeat(1024 * 1024 + 1);
    let output = tool
        .invoke(
            fixture.invocation(&oversized),
            InvocationScope::new(Security, (), &NeverCancelled),
        )
        .await
        .unwrap();
    assert!(
        output.starts_with("error:"),
        "expected an error result, got: {output}"
    );
    assert_eq!(fixture.notices().await.len(), 0);
}

#[tokio::test]
async fn empty_text_fails_without_appending_a_notice() {
    let fixture = Fixture::new();
    let tool = fixture.tool();
    let output = tool
        .invoke(
            fixture.invocation(""),
            InvocationScope::new(Security, (), &NeverCancelled),
        )
        .await
        .unwrap();
    assert!(output.starts_with("error:"), "got: {output}");
    assert_eq!(fixture.notices().await.len(), 0);
}

#[tokio::test]
async fn two_copies_append_two_notices() {
    let fixture = Fixture::new();
    let tool = fixture.tool();
    for text in ["first", "second"] {
        let output = tool
            .invoke(
                fixture.invocation(text),
                InvocationScope::new(Security, (), &NeverCancelled),
            )
            .await
            .unwrap();
        assert!(output.starts_with("Copied "));
    }
    assert_eq!(fixture.notices().await.len(), 2);
}
