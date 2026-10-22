//! Bounded user-facade tests. Full constructor proofs also require scope composition.
use super::*;
use parrot_domain::contracts::{
    async_ops::OperationResult,
    filesystem::{
        AuthorizedReadService, BoundRead, FileMetadata, FileReadError, FileReadErrorKind,
        LinkPolicy, ReadRequest,
    },
    paths::ResolvedRead,
    security::AccessSnapshot,
};
use parrot_domain::values::config::{PathPlatform, SkillConfiguration};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouting},
        registry::ProviderRegistry,
    },
    sessions::{Content, Error, EventStream, Provider, Request, Session},
};
use parrot_runtime::prompt_context::{AgentIdentityDescription, SkillDiscoveryContext};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
};

fn event(id: &str) -> wire::Event {
    wire::Event {
        id: id.into(),
        ..wire::Event::new()
    }
}
fn stream(
    broker: Arc<EventBroker>,
    replay: Vec<wire::Event>,
    initial: Vec<wire::Event>,
    cursor: Option<String>,
) -> UserStream {
    let pending = Box::pin(async move {
        let subscription = broker
            .listen(
                cursor.as_deref(),
                move || async move { Ok::<_, RpcError>(replay) },
                move || async move { Ok::<_, RpcError>(initial) },
            )
            .await?;
        Ok(Arc::new(subscription) as Arc<UserSubscription>)
    });
    UserStream {
        pending: Some(pending),
        subscription: None,
        cancellation: Arc::new(Signal::new()),
    }
}

#[tokio::test]
async fn actual_broker_listen_replays_then_snapshots_then_deduplicated_live_and_does_not_end_at_turn()
 {
    let broker = Arc::new(EventBroker::new());
    let captured = broker.clone();
    let pending = Box::pin(async move {
        let during_replay = captured.clone();
        let subscription = captured
            .listen(
                Some("anchor"),
                move || async move {
                    during_replay.publish_retained(&event("replayed"));
                    during_replay.publish_retained(&event("live"));
                    Ok::<_, RpcError>(vec![event("anchor"), event("replayed")])
                },
                || async { Ok::<_, RpcError>(vec![event("snapshot")]) },
            )
            .await?;
        Ok(Arc::new(subscription) as Arc<UserSubscription>)
    });
    let mut stream = UserStream {
        pending: Some(pending),
        subscription: None,
        cancellation: Arc::new(Signal::new()),
    };
    let cancel = Signal::new();
    for expected in ["replayed", "snapshot", "live"] {
        assert_eq!(stream.next(&cancel).await.unwrap().unwrap().id, expected);
    }
    let mut ended = event("ended");
    ended.set_turn_ended(wire::TurnEnded::new());
    broker.publish_retained(&ended);
    assert!(
        stream
            .next(&cancel)
            .await
            .unwrap()
            .unwrap()
            .has_turn_ended()
    );
    broker.publish_retained(&event("after-ended"));
    assert_eq!(
        stream.next(&cancel).await.unwrap().unwrap().id,
        "after-ended"
    );
    broker.complete();
    assert!(stream.next(&cancel).await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_replay_anchor_falls_back_to_all_and_preserves_repeated_persisted_ids() {
    let broker = Arc::new(EventBroker::new());
    let mut stream = stream(
        broker.clone(),
        vec![event("same"), event("same"), event("tail")],
        vec![event("inventory")],
        Some("missing".into()),
    );
    let cancel = Signal::new();
    for expected in ["same", "same", "tail", "inventory"] {
        assert_eq!(stream.next(&cancel).await.unwrap().unwrap().id, expected);
    }
    broker.complete();
    assert!(stream.next(&cancel).await.unwrap().is_none());
}

#[tokio::test]
async fn canceled_next_preserves_pending_initialization_and_subscription_lifetime() {
    let started = Arc::new(AtomicUsize::new(0));
    let (send, receive) = oneshot::channel();
    let broker = Arc::new(EventBroker::new());
    let input = broker.clone();
    let count = started.clone();
    let pending = Box::pin(async move {
        count.fetch_add(1, Ordering::SeqCst);
        receive.await.map_err(internal)?;
        let subscription = input
            .listen(
                None,
                || async { Ok::<_, RpcError>(Vec::new()) },
                || async { Ok::<_, RpcError>(vec![event("initial")]) },
            )
            .await?;
        Ok(Arc::new(subscription) as Arc<UserSubscription>)
    });
    let mut stream = UserStream {
        pending: Some(pending),
        subscription: None,
        cancellation: Arc::new(Signal::new()),
    };
    let cancel = Signal::new();
    cancel.cancel();
    assert_eq!(
        stream.next(&cancel).await.unwrap_err().code,
        RpcCode::Cancelled
    );
    assert_eq!(started.load(Ordering::SeqCst), 1);
    send.send(()).unwrap();
    assert_eq!(
        stream.next(&Signal::new()).await.unwrap().unwrap().id,
        "initial"
    );
    assert_eq!(started.load(Ordering::SeqCst), 1);
    broker.complete();
    assert!(stream.next(&Signal::new()).await.unwrap().is_none());
}

#[derive(Clone)]
struct TestSecurity;
impl AccessSnapshot for TestSecurity {
    type Error = FileReadError;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), FileReadError> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), FileReadError> {
        Err(FileReadError::new(
            FileReadErrorKind::Denied,
            "not authorized",
        ))
    }
}
impl RestrictableAccessSnapshot for TestSecurity {
    fn restrict_with(&self, _: &Self) -> Self {
        self.clone()
    }
}
struct EmptyReader;
impl AuthorizedReadService for EmptyReader {
    type Security = TestSecurity;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a TestSecurity,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        Box::pin(async {
            Err(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::NotFound,
                "absent",
            )))
        })
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a TestSecurity,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn read<'a>(
        &'a self,
        _: &'a ReadRequest,
        _: &'a TestSecurity,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async {
            Err(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::NotFound,
                "absent",
            )))
        })
    }
}
struct LocalProvider;
impl Provider for LocalProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Err(Error::InvalidOperation(
            "no provider call in this test".into(),
        ))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("model", "local")]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(self.seed_models()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Err(Error::InvalidOperation(
            "no provider call in this test".into(),
        ))
    }
}
fn selected() -> ResolvedModelSelection {
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(LocalProvider)],
            BTreeMap::from([("local".into(), vec![Model::new("model", "local")])]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), Vec::new()).unwrap());
    let routing = Arc::new(ModelRouting::new(aliases, "local/model"));
    ModelRouter::new(registry, routing).resolve("").unwrap()
}
fn templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([("HOME".into(), "/synthetic/home".into())]),
        )
        .unwrap()
        .1,
    )
}

#[tokio::test]
async fn user_skill_binding_shares_actual_agent_skills_with_its_prompt_product() {
    let reader = Arc::new(EmptyReader);
    let catalog = Arc::new(SkillCatalog::new(
        reader.clone(),
        Vec::new(),
        SkillDiscoveryContext {
            working_directory: "/synthetic/workspace".into(),
            platform: PathPlatform::Unix,
        },
        Arc::new(|| (SkillConfiguration::default(), 0)),
    ));
    let service = CatalogUserSkills {
        catalog,
        reader,
        templates: templates(),
        working_directory: "/synthetic/workspace".into(),
    };
    let selection = Arc::new(SelectionCell::new(
        parrot_runtime::agent_turn::services::TurnSelection {
            model: selected(),
            profile: parrot_runtime::agent_turn::services::TurnProfile {
                id: "build".into(),
                maximum_requests: 5,
                allowed_tools: None,
                disabled_tools: Vec::new(),
            },
            security: Arc::new(TestSecurity),
        },
        PolicyLineage::root(),
    ));
    let bindings = service.new_agent(selection).await.unwrap();
    let mut prompt = bindings
        .prompt
        .materialize(&AgentIdentityDescription::default())
        .unwrap();
    assert!(
        prompt.build(&selected()).is_err(),
        "catalog has not yet been sampled"
    );
    bindings.skills.begin_turn(&Signal::new()).await.unwrap();
    assert!(
        prompt.build(&selected()).is_ok(),
        "same AgentSkills instance published its catalog view"
    );
    bindings.skills.end_turn().await;
    let snapshot = service
        .refresh(&TestSecurity, &Signal::new())
        .await
        .unwrap();
    assert!(snapshot.skills.is_empty());
    assert!(snapshot.errors.is_empty());
}

#[test]
fn skill_rpc_error_categories_preserve_cancel_denial_and_not_found() {
    assert_eq!(
        skill_error(OperationError::Cancelled).code,
        RpcCode::Cancelled
    );
    assert_eq!(
        skill_error(OperationError::Failed(FileReadError::new(
            FileReadErrorKind::Denied,
            "denied"
        )))
        .code,
        RpcCode::PermissionDenied
    );
    assert_eq!(
        skill_error(OperationError::Failed(FileReadError::new(
            FileReadErrorKind::NotFound,
            "missing"
        )))
        .code,
        RpcCode::NotFound
    );
}

#[test]
fn actual_lease_runtime_constructs_and_joins_nested_work_after_caller_runtime_drop() {
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let lease = caller.block_on(real_lease());
    let owner = Arc::new(OwnedMutations::new(lease.runtime_handle()));
    let (release, barrier) = oneshot::channel();
    let (finished, completion) = std::sync::mpsc::channel();
    let observer = {
        let _caller = caller.enter();
        owner.observe(move || {
            let nested = tokio::spawn(async move {
                barrier.await.unwrap();
                finished.send(()).unwrap();
            });
            async move {
                nested.await.unwrap();
            }
        })
    };
    drop(observer);
    drop(caller);
    release.send(()).unwrap();
    completion
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("lease driver must keep actual nested work running");
    owner.stop();
    let observer_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer_runtime.block_on(async {
        owner.join().await.unwrap();
        lease.confirm_producers_settled();
        lease.close().await.unwrap();
    });
}

#[tokio::test]
async fn dropped_close_join_observer_preserves_receipt_and_caches_completed_join() {
    let (send, receipt) = oneshot::channel();
    let mut close = HostedCloseJoin {
        receipt: Some(receipt),
        result: None,
    };
    {
        let waiting = close.join();
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(
                waiting.as_mut().poll(context).is_pending()
            ))
            .await
        );
    }
    send.send(Ok(())).unwrap();
    close.join().await.unwrap();
    close.join().await.unwrap();
    assert!(close.receipt.is_none());
}

#[tokio::test]
async fn actual_close_task_panic_receipt_remains_a_stable_repeated_join_failure() {
    let (send, receipt) = oneshot::channel();
    let task = tokio::spawn(async {
        panic!("synthetic actual close producer panic");
    });
    let joined = task.await;
    assert!(joined.as_ref().unwrap_err().is_panic());
    send.send(joined).unwrap();
    let mut close = HostedCloseJoin {
        receipt: Some(receipt),
        result: None,
    };
    let failure = close.join().await.unwrap_err();
    assert_eq!(close.join().await.unwrap_err(), failure);
    assert!(close.receipt.is_none());
}

#[tokio::test]
async fn admitted_mutation_survives_dropped_observer_and_close_joins_actual_producer() {
    let owner = OwnedMutations::new(tokio::runtime::Handle::current());
    let (started, entered) = oneshot::channel();
    let (release, barrier) = oneshot::channel();
    let complete = Arc::new(AtomicUsize::new(0));
    let actual = complete.clone();
    let observer = owner.observe(move || async move {
        started.send(()).unwrap();
        barrier.await.unwrap();
        actual.fetch_add(1, Ordering::SeqCst);
    });
    entered.await.unwrap();
    drop(observer);
    owner.stop();
    assert_eq!(
        owner.observe(|| async {}).await.unwrap_err().code,
        RpcCode::Unavailable
    );
    let joining = owner.join();
    tokio::pin!(joining);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(
            joining.as_mut().poll(context).is_pending()
        ))
        .await
    );
    assert_eq!(complete.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    joining.await.unwrap();
    assert_eq!(complete.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_join_observer_keeps_actual_producer_join_in_its_owner() {
    let owner = OwnedMutations::new(tokio::runtime::Handle::current());
    let (release, barrier) = oneshot::channel();
    let observer = owner.observe(move || async move {
        barrier.await.unwrap();
        7
    });
    owner.stop();
    {
        let joining = owner.join();
        tokio::pin!(joining);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(
                joining.as_mut().poll(context).is_pending()
            ))
            .await
        );
    }
    release.send(()).unwrap();
    assert_eq!(observer.await.unwrap(), 7);
    owner.join().await.unwrap();
}

async fn real_lease() -> Arc<SessionResourceLease> {
    use parrot_domain::values::identifiers::UserSessionId;
    use parrot_storage::{
        diagnostics::FileDiagnosticLog,
        history_files::SessionIndex,
        ownership::{
            OwnershipRoots, ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity,
            SessionAdmissions,
        },
    };
    let root = std::env::temp_dir().join(format!("t62-user-{}", Identifier::event_id().unwrap()));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let runtime = RuntimeIdentity::new(
        "host".into(),
        "boot".into(),
        123,
        "start".into(),
        "runtime".into(),
        Arc::new(|_| ProcessIdentity {
            status: ProcessIdentityStatus::Found,
            process_start_token: Some("start".into()),
        }),
    )
    .with_proven_comparable_process_start_tokens();
    let admissions = SessionAdmissions::new(
        OwnershipRoots::new(root.join("new"), root.join("old")).unwrap(),
        runtime,
        true,
    );
    let id = UserSessionId::parse("user-session-test").unwrap();
    let mut admitted = admissions.create_fresh(&workspace, &id).unwrap();
    let selected = admitted.selected_root.take().unwrap();
    let activation = admitted.admission.activation_lease.take().unwrap();
    let diagnostics = FileDiagnosticLog::open_session(
        selected.path().join("session.log"),
        "user-test",
        id.value(),
        |_| {},
    )
    .unwrap();
    SessionResourceLease::open(
        SessionIndex::new(selected, id, &workspace).unwrap(),
        activation,
        diagnostics,
    )
    .await
    .unwrap()
}

async fn actual_histories(lease: Arc<SessionResourceLease>) -> Histories<TestSecurity> {
    use parrot_domain::values::identifiers::UserSessionId;
    let workspace = lease.selected_root().path().join("synthetic-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let resources = Arc::new(
        UserSessionResources::new(
            lease.selected_root().clone(),
            UserSessionId::parse("user-session-test").unwrap(),
            ProjectWorkspace::from_resolved(workspace.clone(), workspace, None, None),
        )
        .unwrap(),
    );
    let catalog = templates();
    let reader = Arc::new(EmptyReader);
    let skills = Arc::new(CatalogUserSkills {
        catalog: Arc::new(SkillCatalog::new(
            reader.clone(),
            Vec::new(),
            SkillDiscoveryContext {
                working_directory: resources.workspace().launch_directory().to_path_buf(),
                platform: PathPlatform::Unix,
            },
            Arc::new(|| (SkillConfiguration::default(), 0)),
        )),
        reader,
        templates: catalog.clone(),
        working_directory: resources
            .workspace()
            .launch_directory()
            .to_string_lossy()
            .into_owned(),
    });
    let broker = Arc::new(EventBroker::new());
    let diagnostics = Arc::new(NoInteractionDiagnostics);
    let permissions =
        durable_permission_factory(None, diagnostics.clone())(lease.clone(), broker.clone(), true)
            .unwrap();
    let services = Arc::new(UserServices {
        id: "user-session-test".into(),
        lease: lease.clone(),
        resources,
        broker,
        templates: catalog.clone(),
        questions: Arc::new(QuestionBroker::new(None, diagnostics).unwrap()),
        asks: Arc::new(parrot_runtime::interactions::AgentAskBroker::new()),
        permissions,
        skills,
        lifetime: Arc::new(Signal::new()),
        standing: StandingInstruction::new(catalog.clone()),
        statistics: lease.events.get_runtime_statistics().await.unwrap(),
        root_identity: AgentIdentity::root("agent-first".into(), "main".into(), catalog),
        modes: Arc::new(
            UserSessionModes::new(
                Arc::new(
                    ModeRegistry::new(
                        vec![
                            parrot_runtime::plan_mode::MaterializedModeProfile::new(
                                "build".into(),
                                &parrot_foundation::config::runtime::ProfileConfig {
                                    prompt: String::new(),
                                    usage: String::new(),
                                    allowed_tools: None,
                                    max_turns: 8,
                                    recursion_limit: 5,
                                    read_only: false,
                                    is_user_selectable: true,
                                    is_agent_selectable: false,
                                    sandbox_rules: Vec::new(),
                                },
                                &[],
                                Arc::new(TestSecurity),
                            )
                            .unwrap(),
                        ],
                        "build",
                    )
                    .unwrap(),
                ),
                Vec::new(),
            )
            .unwrap(),
        ),
        registry: OnceLock::new(),
    });
    Histories {
        services,
        repository: ConversationRepository::new(lease.events.clone()),
    }
}

async fn record_history_occupant(histories: &Histories<TestSecurity>, id: &str, text: &str) {
    let mut started = wire::Event::new();
    started.id = Identifier::event_id().unwrap();
    started.agent_session_id = id.into();
    started.set_agent_started(wire::AgentStarted {
        name: "main".into(),
        ..wire::AgentStarted::new()
    });
    histories
        .services
        .lease
        .events
        .append(started, None)
        .await
        .unwrap();
    let mut message = wire::Event::new();
    message.id = Identifier::event_id().unwrap();
    message.agent_session_id = id.into();
    histories
        .repository
        .append(message, Some("user".into()), Some(text.into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_history_preparation_queries_new_lineage_after_same_name_reuse() {
    let lease = real_lease().await;
    let histories = actual_histories(lease.clone()).await;
    record_history_occupant(&histories, "agent-first", "first-occupant-message").await;
    let first = AgentIdentity::root(
        "agent-first".into(),
        "main".into(),
        histories.services.templates.clone(),
    );
    histories
        .prepare(&first)
        .await
        .unwrap()
        .refresh()
        .await
        .unwrap();
    record_history_occupant(&histories, "agent-second", "second-occupant-message").await;
    let second = AgentIdentity::root(
        "agent-second".into(),
        "main".into(),
        histories.services.templates.clone(),
    );
    histories
        .prepare(&second)
        .await
        .unwrap()
        .refresh()
        .await
        .unwrap();
    let scratch = histories
        .services
        .resources
        .agent_scratch(&["main"])
        .unwrap();
    let physical = std::fs::read_to_string(scratch.history_path()).unwrap();
    assert!(physical.contains("first-occupant-message"));
    assert!(physical.contains("second-occupant-message"));
    histories.services.permissions.close().await;
    histories.services.broker.complete();
    lease.close().await.unwrap();
}

#[tokio::test]
async fn refresh_all_histories_preserves_occupant_order_and_source_failure_boundaries() {
    let lease = real_lease().await;
    let histories = actual_histories(lease.clone()).await;
    record_history_occupant(&histories, "agent-first", "first-occupant-message").await;
    record_history_occupant(&histories, "agent-second", "second-occupant-message").await;
    let mut orphan = event(&Identifier::event_id().unwrap());
    orphan.agent_session_id = "agent-without-lineage".into();
    histories
        .repository
        .append(
            orphan,
            Some("user".into()),
            Some("unresolved-history".into()),
        )
        .await
        .unwrap();
    let path = histories
        .services
        .resources
        .agent_scratch(&["main"])
        .unwrap()
        .history_path();
    // A directory at a derived-file path is replaced, not treated as authority.
    std::fs::create_dir(&path).unwrap();
    histories.refresh_all().await.unwrap();
    assert!(path.is_file());
    let physical = std::fs::read_to_string(&path).unwrap();
    assert!(
        physical.find("first-occupant-message").unwrap()
            < physical.find("second-occupant-message").unwrap()
    );
    assert!(!physical.contains("unresolved-history"));
    let log = std::fs::read_to_string(lease.selected_root().path().join("session.log")).unwrap();
    assert!(log.contains("agent_history_unresolved"));
    assert!(log.contains("agent-without-lineage"));
    histories.services.permissions.close().await;
    histories.services.broker.complete();
    lease.close().await.unwrap();
    // Unlike projection I/O, failure to enumerate authoritative lineage is visible.
    assert!(histories.refresh_all().await.is_err());
}

#[tokio::test]
async fn dropping_actual_history_query_observer_allows_fresh_preparation_after_actor_settles() {
    let lease = real_lease().await;
    let histories = actual_histories(lease.clone()).await;
    record_history_occupant(&histories, "agent-first", "retained-history").await;
    let identity = AgentIdentity::root(
        "agent-first".into(),
        "main".into(),
        histories.services.templates.clone(),
    );
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, entered) = oneshot::channel();
    let barrier = lease
        .database
        .admit(
            "history-query-barrier".to_owned(),
            &NeverCancelled,
            move |_| {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            },
        )
        .unwrap();
    entered.await.unwrap();
    {
        let preparing = histories.prepare(&identity);
        tokio::pin!(preparing);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(
                preparing.as_mut().poll(context).is_pending()
            ))
            .await
        );
    }
    release.send(()).unwrap();
    barrier.into_completion().await.unwrap();
    histories
        .prepare(&identity)
        .await
        .unwrap()
        .refresh()
        .await
        .unwrap();
    let scratch = histories
        .services
        .resources
        .agent_scratch(&["main"])
        .unwrap();
    assert!(
        std::fs::read_to_string(scratch.history_path())
            .unwrap()
            .contains("retained-history")
    );
    histories.services.permissions.close().await;
    histories.services.broker.complete();
    lease.close().await.unwrap();
}

struct NoInteractionDiagnostics;
impl InteractionDiagnostics for NoInteractionDiagnostics {
    fn write(&self, _: parrot_runtime::interactions::BrokerDiagnostic) {}
}

#[tokio::test]
async fn actual_permission_adapter_publishes_exact_durably_stored_event() {
    use parrot_runtime::interactions::{PERMISSION_CHOICES, PendingPermission, PermissionEvents};
    let lease = real_lease().await;
    let broker = Arc::new(EventBroker::new());
    let events = DurablePermissionEvents {
        lease: lease.clone(),
        broker: broker.clone(),
    };
    let subscription = broker
        .listen(
            None,
            || async { Ok::<_, RpcError>(Vec::new()) },
            || async { Ok::<_, RpcError>(Vec::new()) },
        )
        .await
        .unwrap();
    let pending = PendingPermission {
        id: "permission-test".into(),
        agent_session_id: "agent-test".into(),
        reason: "synthetic reason".into(),
        targets: vec![WriteTarget {
            path: "/synthetic/file".into(),
            kind: WriteTargetKind::File,
        }],
        choices: PERMISSION_CHOICES.to_vec(),
    };
    let committed = events.append(&pending).await.unwrap();
    let persisted = lease.events.replay().await.unwrap();
    assert_eq!(persisted, vec![committed.value().clone()]);
    events.publish(&committed).await.unwrap();
    assert_eq!(
        subscription.next(&Signal::new()).await.unwrap().unwrap(),
        *committed.value()
    );
    assert_eq!(committed.value().permission_pending().id, pending.id);
    broker.complete();
    lease.close().await.unwrap();
}

struct ObservedPermissionEvents {
    actual: DurablePermissionEvents,
    entered: Mutex<Option<oneshot::Sender<()>>>,
}
impl parrot_runtime::interactions::PermissionEvents<WriteTarget> for ObservedPermissionEvents {
    fn append<'a>(
        &'a self,
        pending: &'a parrot_runtime::interactions::PendingPermission<WriteTarget>,
    ) -> BoxFuture<'a, Result<Committed<wire::Event>, InteractionError>> {
        Box::pin(async move {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
            }
            self.actual.append(pending).await
        })
    }
    fn publish<'a>(
        &'a self,
        event: &'a Committed<wire::Event>,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        self.actual.publish(event)
    }
}
struct NeverApprove;
impl parrot_runtime::interactions::TargetApproval<WriteTarget> for NeverApprove {
    fn validate(&self, _: &[WriteTarget]) -> Result<(), InteractionError> {
        Ok(())
    }
    fn approve(&self, _: &[WriteTarget]) -> Result<(), InteractionError> {
        panic!("this synthetic test never grants access")
    }
}

#[tokio::test]
async fn dropped_request_observer_keeps_actual_append_publication_and_pending_close_joined() {
    use parrot_runtime::interactions::PermissionRequest;
    let lease = real_lease().await;
    let broker = Arc::new(EventBroker::new());
    let subscription = broker
        .listen(
            None,
            || async { Ok::<_, RpcError>(Vec::new()) },
            || async { Ok::<_, RpcError>(Vec::new()) },
        )
        .await
        .unwrap();
    let (entered, append_entered) = oneshot::channel();
    let permissions = Arc::new(
        PermissionBroker::new(
            true,
            None,
            Arc::new(NoInteractionDiagnostics),
            Arc::new(ObservedPermissionEvents {
                actual: DurablePermissionEvents {
                    lease: lease.clone(),
                    broker: broker.clone(),
                },
                entered: Mutex::new(Some(entered)),
            }),
        )
        .unwrap(),
    );
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, database_entered) = oneshot::channel();
    let blocker = lease
        .database
        .admit(
            "permission-test-barrier".to_owned(),
            &NeverCancelled,
            move |_| {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            },
        )
        .unwrap();
    database_entered.await.unwrap();
    let owner = OwnedMutations::new(tokio::runtime::Handle::current());
    let producer = permissions.clone();
    let observer = owner.observe(move || async move {
        producer
            .request(
                PermissionRequest {
                    agent_session_id: "agent-test".into(),
                    reason: "synthetic request".into(),
                    targets: vec![WriteTarget {
                        path: "/synthetic/file".into(),
                        kind: WriteTargetKind::File,
                    }],
                    approval: Arc::new(NeverApprove),
                },
                &Signal::new(),
            )
            .await
    });
    append_entered.await.unwrap();
    drop(observer);
    owner.stop();
    let closing = permissions.close();
    tokio::pin!(closing);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(
            closing.as_mut().poll(context).is_pending()
        ))
        .await
    );
    release.send(()).unwrap();
    blocker.into_completion().await.unwrap();
    closing.await;
    owner.join().await.unwrap();
    let published = subscription.next(&Signal::new()).await.unwrap().unwrap();
    assert!(published.has_permission_pending());
    assert_eq!(lease.events.replay().await.unwrap(), vec![published]);
    assert!(permissions.pending().await.is_empty());
    assert!(
        !lease.is_cancelled(),
        "actual repository stays available until the producer join"
    );
    broker.complete();
    assert!(subscription.next(&Signal::new()).await.unwrap().is_none());
    lease.close().await.unwrap();
}

#[tokio::test]
async fn producer_panic_is_not_reported_as_successful_settlement() {
    let owner = OwnedMutations::new(tokio::runtime::Handle::current());
    let observer = owner.observe(|| async { panic!("synthetic producer failure") });
    assert!(observer.await.is_err());
    owner.stop();
    assert!(owner.join().await.is_err());
    assert_eq!(
        owner.synchronous(|| Ok(())).unwrap_err().code,
        RpcCode::Unavailable
    );
}
