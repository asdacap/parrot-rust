use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        paths::{RootOrigin, SelectedSessionRoot},
    },
    protocol::messages::parrot::{AgentStarted, Event},
    values::accounting::{AgentSessionStatistics, AgentUsageKey},
};
use parrot_provider::{
    catalogs::{Model, ModelMetadataFields, ModelVariant},
    routing::registry::ProviderModel,
    sessions::{
        Completion, Content, Error, EventStream, Provider, Reasoning, Request, Session, ToolCall,
    },
};
use parrot_runtime::{agent_turn::usage::SessionUsageRecorder, event_broker::EventBroker};
use parrot_storage::{
    conversation::{AgentHistoryView, ConversationRepository},
    database::SessionDatabase,
    events::{EventError, EventRepository},
    history_files::AgentHistoryFile,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t46-usage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn repository(&self) -> ConversationRepository {
        let root = SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap();
        ConversationRepository::new(EventRepository::new(SessionDatabase::open(root).unwrap()))
    }
    fn history(&self, repository: &ConversationRepository, agent: &str) -> AgentHistoryView {
        repository
            .bind_agent_history(
                agent.into(),
                Arc::new(
                    AgentHistoryFile::new(
                        repository.events().database().selected_root().clone(),
                        self.0.join(format!("{agent}/history.jsonl")),
                        vec![agent.into()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct UnusedProvider;
impl Provider for UnusedProvider {
    fn id(&self) -> &str {
        "actual-provider"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        panic!("usage must not call provider")
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        panic!("usage must not read credentials")
    }
    fn seed_models(&self) -> Vec<Model> {
        panic!("usage must not enumerate models")
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        panic!("usage must not refresh models")
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        panic!("usage must not estimate images")
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        panic!("usage must not open provider session")
    }
}
fn model() -> ProviderModel {
    let mut model = Model::new("vendor/model", "metadata-provider-not-used");
    model.context_window = 4096;
    model.input_price = 0.25;
    model.cached_input_price = 0.0;
    model.output_price = 0.5;
    ProviderModel {
        provider: Arc::new(UnusedProvider),
        model,
        variant: Some(ModelVariant::new("variant-not-used", "high")),
    }
}
fn completion(input_tokens: i32, output_tokens: i32) -> Completion {
    Completion {
        input_tokens,
        cached_input_tokens: 3,
        output_tokens,
        ..Completion::default()
    }
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments_json: "{}".into(),
    }
}
async fn started(repository: &ConversationRepository, agent: &str, parent: &str) {
    let mut event = Event {
        id: format!("start-{agent}"),
        agent_session_id: agent.into(),
        ..Event::new()
    };
    event.set_agent_started(AgentStarted {
        parent_agent_session_id: parent.into(),
        ..AgentStarted::new()
    });
    repository.events().append(event, None).await.unwrap();
}
async fn root_recorder(
    fixture: &Fixture,
    repository: &ConversationRepository,
    broker: Arc<EventBroker>,
) -> SessionUsageRecorder {
    SessionUsageRecorder::new(
        fixture.history(repository, "root"),
        String::new(),
        broker,
        Vec::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn actual_sqlite_request_facts_reuse_live_stats_and_rehydrate_exact_usage() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    started(&repository, "root", "").await;
    let broker = Arc::new(EventBroker::new());
    let listener = broker.subscribe();
    let recorder = root_recorder(&fixture, &repository, broker.clone()).await;
    let selected = model();
    let mut first = completion(17, 9);
    first.tool_calls = vec![call("call")];
    recorder
        .record_request(&selected, None, &first)
        .await
        .unwrap();
    let own = recorder.capture();
    assert_eq!(
        (own.own.totals.input_tokens, own.own.totals.output_tokens),
        (17, 9)
    );
    assert_eq!((own.context_size, own.context_limit), (17, 4096));
    assert_eq!(own.own.totals.input_cost, 17.0 * 0.25);
    let same = root_recorder(&fixture, &repository, broker).await;
    assert_eq!(same.capture(), own);
    let effort = Reasoning {
        effort: String::new(),
        summary: "auto".into(),
    };
    same.record_request(&selected, Some(&effort), &completion(23, 11))
        .await
        .unwrap();
    let persisted = repository.events().replay().await.unwrap();
    let requests: Vec<_> = persisted
        .iter()
        .filter(|event| event.has_request_usage_recorded())
        .map(Event::request_usage_recorded)
        .collect();
    assert_eq!(
        requests
            .iter()
            .map(|r| format!("{}:{}", r.input_tokens, r.output_tokens))
            .collect::<Vec<_>>()
            .join("|"),
        "17:9|23:11"
    );
    assert_eq!(requests[0].provider, "actual-provider");
    assert_eq!(requests[0].model, "vendor/model");
    assert_eq!(requests[0].effort, None);
    assert_eq!(requests[1].effort, Some(String::new()));
    assert_eq!(requests[0].tool_call_ids, ["call"]);
    assert!(!requests[0].request_id.is_empty());
    assert_ne!(requests[0].request_id, requests[1].request_id);
    assert!(!persisted.iter().any(|event| event.has_agent_statistics_updated() || event.has_session_usage_snapshot()));
    let publications: Vec<_> = std::iter::from_fn(|| listener.try_next()).collect();
    assert_eq!(publications.len(), 6);
    for (index, batch) in publications.chunks_exact(3).enumerate() {
        assert!(batch[0].has_request_usage_recorded());
        assert!(batch[1].has_agent_statistics_updated());
        assert!(batch[2].has_session_usage_snapshot());
        assert!(persisted.iter().any(|fact| fact.id == batch[0].id));
        assert_eq!(
            batch[1].agent_statistics_updated().input_tokens,
            if index == 0 { 17 } else { 40 }
        );
    }
    let expected = recorder.capture();
    let usage = recorder.usage_snapshot();
    assert_eq!((usage.input_tokens, usage.output_tokens), (40, 20));
    assert_eq!(
        usage,
        publications
            .last()
            .unwrap()
            .session_usage_snapshot()
            .clone()
    );
    repository.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    let restored = root_recorder(&fixture, &reopened, Arc::new(EventBroker::new())).await;
    assert_eq!(restored.capture(), expected);
    assert_eq!(restored.usage_snapshot(), usage);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn cached_price_presence_raw_negative_totals_and_zero_context_are_preserved() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let recorder = root_recorder(&fixture, &repository, Arc::new(EventBroker::new())).await;
    let mut selected = model();
    recorder
        .record_request(&selected, None, &completion(17, 9))
        .await
        .unwrap();
    selected
        .model
        .fields
        .insert(ModelMetadataFields::CACHED_INPUT_PRICE);
    recorder
        .record_request(&selected, None, &completion(17, 9))
        .await
        .unwrap();
    selected.model.context_window = 0;
    recorder
        .record_request(
            &selected,
            None,
            &Completion {
                input_tokens: -2,
                cached_input_tokens: -3,
                output_tokens: -4,
                ..Completion::default()
            },
        )
        .await
        .unwrap();
    recorder
        .record_request(&selected, None, &Completion::default())
        .await
        .unwrap();
    let persisted = repository.events().replay().await.unwrap();
    let requests: Vec<_> = persisted
        .iter()
        .map(Event::request_usage_recorded)
        .collect();
    assert_eq!(requests[0].input_cost, 17.0 * 0.25);
    assert_eq!(requests[1].input_cost, 14.0 * 0.25);
    assert_eq!(
        (
            requests[2].input_tokens,
            requests[2].cached_input_tokens,
            requests[2].output_tokens
        ),
        (-2, -3, -4)
    );
    assert_eq!(
        (requests[2].input_cost, requests[2].output_cost),
        (0.0, -2.0)
    );
    assert_eq!(
        (requests[2].context_size, requests[2].context_limit),
        (-2, 0)
    );
    assert_eq!(
        (
            recorder.capture().context_size,
            recorder.capture().context_limit
        ),
        (0, 0)
    );
    assert_eq!(recorder.capture().own.totals.input_tokens, 32);
    repository.events().database().close().await.unwrap();
}

#[tokio::test]
async fn tool_facts_link_actual_requests_are_deduplicated_and_keep_context() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let broker = Arc::new(EventBroker::new());
    let listener = broker.subscribe();
    let recorder = root_recorder(&fixture, &repository, broker).await;
    let mut completed = completion(17, 9);
    completed.tool_calls = vec![call("linked")];
    let effort = Reasoning {
        effort: "low".into(),
        summary: "auto".into(),
    };
    recorder
        .record_request(&model(), Some(&effort), &completed)
        .await
        .unwrap();
    recorder.record_tool(23, &call("linked")).await.unwrap();
    recorder.record_tool(23, &call("linked")).await.unwrap();
    recorder.record_tool(24, &call("legacy")).await.unwrap();
    let persisted = repository.events().replay().await.unwrap();
    assert_eq!(persisted.len(), 3);
    let request = persisted[0].request_usage_recorded();
    assert_eq!(persisted[1].id, "tool-usage:root:linked");
    let linked = persisted[1].tool_execution_started();
    assert_eq!(linked.request_id, request.request_id);
    assert_eq!(
        (linked.assistant_sequence, linked.tool_name.as_str()),
        (23, "read")
    );
    assert_eq!(
        (
            linked.provider.as_str(),
            linked.model.as_str(),
            linked.effort.as_deref()
        ),
        ("actual-provider", "vendor/model", Some("low"))
    );
    let legacy = persisted[2].tool_execution_started();
    assert_eq!(
        (
            legacy.request_id.as_str(),
            legacy.provider.as_str(),
            legacy.model.as_str(),
            legacy.effort.as_deref()
        ),
        ("", "unknown", "legacy", None)
    );
    assert_eq!(recorder.capture().own.totals.tool_calls, 2);
    assert_eq!(recorder.capture().context_size, 17);
    assert_eq!(recorder.capture().context_limit, 4096);
    assert_eq!(std::iter::from_fn(|| listener.try_next()).count(), 9);
    let expected = recorder.capture();
    repository.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    let restored = root_recorder(&fixture, &reopened, Arc::new(EventBroker::new())).await;
    assert_eq!(restored.capture(), expected);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn actual_parent_chain_receives_cumulative_not_own_or_context_usage_and_replays() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    for (agent, parent) in [("root", ""), ("parent", "root"), ("leaf", "parent")] {
        started(&repository, agent, parent).await;
    }
    let coordinator = repository.events().get_runtime_statistics().await.unwrap();
    let root = coordinator.get_agent_statistics("root");
    let parent = coordinator.get_agent_statistics("parent");
    let ancestors = vec![
        ("parent".into(), parent.clone()),
        ("root".into(), root.clone()),
    ];
    let recorder = SessionUsageRecorder::new(
        fixture.history(&repository, "leaf"),
        "parent".into(),
        Arc::new(EventBroker::new()),
        ancestors,
    )
    .await
    .unwrap();
    recorder
        .record_request(&model(), None, &completion(17, 9))
        .await
        .unwrap();
    recorder.record_tool(1, &call("legacy")).await.unwrap();
    for ancestor in [&root, &parent] {
        let snapshot = ancestor.capture();
        assert_eq!(snapshot.own.totals.input_tokens, 0);
        assert_eq!(snapshot.cumulative.totals, recorder.capture().own.totals);
        assert_eq!((snapshot.context_size, snapshot.context_limit), (0, 0));
    }
    assert_eq!(
        recorder.capture().own.models[&AgentUsageKey::legacy()].tool_calls,
        1
    );
    assert_eq!(recorder.usage_snapshot().input_tokens, 17);
    let root_before = root.capture();
    let parent_before = parent.capture();
    let leaf_before = recorder.capture();
    repository.events().database().close().await.unwrap();
    let reopened = fixture.repository();
    let replay = reopened.events().replay_statistics().await.unwrap();
    assert_eq!(replay.agents["root"], root_before);
    assert_eq!(replay.agents["parent"], parent_before);
    assert_eq!(replay.agents["leaf"], leaf_before);
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn foreign_statistics_handles_are_rejected_and_failed_append_does_not_apply_or_publish() {
    let fixture = Fixture::new();
    let repository = fixture.repository();
    let broker = Arc::new(EventBroker::new());
    let foreign = Arc::new(AgentSessionStatistics::default());
    let result = SessionUsageRecorder::new(
        fixture.history(&repository, "leaf"),
        "parent".into(),
        broker.clone(),
        vec![("parent".into(), foreign)],
    )
    .await;
    assert!(matches!(result, Err(EventError::Argument(_))));
    let recorder = root_recorder(&fixture, &repository, broker.clone()).await;
    let listener = broker.subscribe();
    let expected = recorder.capture();
    repository.events().database().close().await.unwrap();
    assert!(
        recorder
            .record_request(&model(), None, &completion(17, 9))
            .await
            .is_err()
    );
    assert_eq!(recorder.capture(), expected);
    assert!(listener.try_next().is_none());
}
