//! T52 through the actual retained scopes, RuntimeBackend, SQLite/JSONL and wait registration.
//! The synthetic process observer is a domain port, not production OS-adapter proof.
use super::support;

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError},
        events::EventPublisher,
        status::{
            ActiveWorkObservation, ActiveWorkState, ProcessStatusSnapshot, ProcessStatusSource,
        },
        storage::Committed,
    },
    protocol::messages::parrot::{self as protocol, Event},
    values::{
        accounting::RetainedAgentBudget,
        agent_tasks::{ExecutionStatus as TaskState, Task, TaskPayload},
        identifiers::UserSessionId,
    },
};
use parrot_foundation::paths::UserSessionResources;
use parrot_provider::sessions;
use parrot_runtime::{
    active_work::{
        blockers::{
            ActiveWorkBlocker, ActiveWorkReminder, ChildBlocker, ProcessBlocker, QueueBlocker,
            TaskBlocker, TaskSnapshotSource,
        },
        completion::SessionCompletion,
        exit_reminders::ExitReminders,
        wait::{WaitDuration, WaitService},
    },
    agent_queues::{
        AgentQueues, QueueChildAdmissionValidator, QueueDiagnostic, QueueDiagnostics,
        QueueInventory,
    },
    agent_turn::{
        engine::{Publisher, RuntimeBackend, RuntimeConfig},
        lifecycle::{
            AgentSession, DrainState, Execution, ExecutionStatus, IncomingActivity, Signal,
        },
        provider_call::ProviderActivity,
        services::*,
    },
    children::*,
    compaction::{ContextResult, ContextSnapshot, ContextStatus},
    event_broker::{BrokerError, EventBroker},
    interactions::*,
    prompt_context::{PromptResult, SkillRendering, SystemPrompt},
};
use parrot_storage::{
    conversation::{AgentHistoryView, MessageRole},
    events::{ConversationPart, NeverCancelled},
    queue_files::{Direction, Owner},
};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use support::{Gate, Histories, Scripted, Security, Tools, bounded, id, templates};

struct Prompt;
impl SystemPrompt for Prompt {
    fn renew_epoch(&mut self) {}
    fn sample_epoch<'a>(&'a mut self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn build(
        &mut self,
        _: &parrot_provider::routing::aliases::ResolvedModelSelection,
    ) -> PromptResult<String> {
        Ok("offline T52 fixture".into())
    }
}
struct Skills;
impl TurnSkills for Skills {
    fn begin_turn<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn select<'a>(
        &'a self,
        _: &'a [ConversationPart],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn render<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<SkillRendering>> {
        Box::pin(async {
            Ok(SkillRendering {
                content: String::new(),
                loaded_paths: vec![],
            })
        })
    }
    fn end_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
struct Status;
impl ContextStatus for Status {
    fn observe<'a>(
        &'a self,
        _: &'a ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn observe_fitted<'a>(
        &'a self,
        estimate: &'a (dyn Fn(&str) -> ContextResult<ContextSnapshot> + Send + Sync),
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, ContextResult<String>> {
        Box::pin(async move {
            estimate("")?;
            Ok(String::new())
        })
    }
}
impl TurnStatus<Security> for Status {
    fn bind(&self, _: &TurnSelection<Security>) -> Arc<dyn ContextStatus> {
        Arc::new(Self)
    }
}
struct Activity;
impl ProviderActivity for Activity {
    fn begin_provider_request(&self) {}
    fn observe_provider_event(&self, _: &sessions::Event) {}
    fn finish_provider_request(&self) {}
}
impl SessionActivity for Activity {
    fn change_state(&self, _: DrainState) {}
    fn begin_execution(&self) -> i64 {
        1
    }
    fn execution_duration(&self) -> Duration {
        Duration::ZERO
    }
    fn finish_execution(&self, _: i64, _: &Execution) {}
    fn turn_started(&self, _: &str) {}
    fn turn_finished(&self, _: &str, _: &str, _: Option<&str>) {}
    fn assistant_message(&self, _: &str) {}
}
struct NoParent;
impl ParentCompletion for NoParent {
    fn deliver<'a>(&'a self, _: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async { Ok(()) })
    }
}
#[derive(Default)]
struct QueueLog;
impl QueueDiagnostics for QueueLog {
    fn record(&self, _: QueueDiagnostic) {}
}

#[derive(Default)]
struct Tasks {
    values: Mutex<Vec<Task>>,
    observations: AtomicUsize,
}
impl TaskSnapshotSource for Tasks {
    fn snapshot(&self) -> BoxFuture<'_, Vec<Task>> {
        Box::pin(async {
            self.observations.fetch_add(1, Ordering::SeqCst);
            self.values.lock().unwrap().clone()
        })
    }
}
#[derive(Default)]
struct Processes(Mutex<Vec<ActiveWorkObservation>>);
impl ProcessStatusSource for Processes {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        self.0.lock().unwrap().clone()
    }
    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        vec![]
    }
}
#[derive(Default)]
struct Questions(Mutex<Vec<Arc<ChildQuestion>>>);
impl ChildQuestionRegistry for Questions {
    fn find(&self, name: &str) -> Option<Arc<ChildQuestion>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|child| child.identity.name == name)
            .cloned()
    }
    fn snapshot(&self) -> Vec<Arc<ChildQuestion>> {
        self.0.lock().unwrap().clone()
    }
}
#[derive(Default)]
struct QuestionDelivery(AtomicUsize);
impl ChildQuestionDelivery for QuestionDelivery {
    fn steer<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct QuestionTemplates;
impl ChildQuestionTemplates for QuestionTemplates {
    fn question(&self, name: &str, questions: &str) -> Result<String, InteractionError> {
        Ok(format!("{name}: {questions}"))
    }
    fn reminder(&self, children: &str) -> Result<String, InteractionError> {
        Ok(format!(
            "Pending child questions:{children}\nReply before finishing."
        ))
    }
}
#[derive(Default)]
struct CallbackBinding(Mutex<Option<Arc<SessionCompletion<Publisher>>>>);
impl TurnCompletion<Security> for CallbackBinding {
    fn complete<'a>(
        &'a self,
        candidate: &'a CompletionCandidate<Security>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<CompletionDecision, TurnError>> {
        Box::pin(async move {
            let callback = self
                .0
                .lock()
                .unwrap()
                .clone()
                .expect("fixture callback must bind before admission");
            callback.complete(candidate, cancellation).await
        })
    }
}
type Backend = RuntimeBackend<Security, Tools>;
struct Built {
    identity: Arc<AgentIdentity>,
    session: AgentSession<Backend>,
    provider: Scripted,
    history: AgentHistoryView,
    queues: Arc<AgentQueues<Security>>,
    broker: Arc<EventBroker>,
    callback: Arc<CallbackBinding>,
    active: Mutex<Option<Arc<ActiveWorkReminder>>>,
    tasks: Arc<Tasks>,
    processes: Arc<Processes>,
    question_registry: Arc<Questions>,
    questions: Arc<ChildQuestionCoordinator>,
    delivery: Arc<QuestionDelivery>,
}
impl Built {
    fn gates(&self, count: usize) -> Vec<Arc<Gate>> {
        let gates = (0..count).map(|_| Gate::new()).collect::<Vec<_>>();
        self.provider
            .gates
            .lock()
            .unwrap()
            .extend(gates.iter().cloned());
        gates
    }
    fn wait_service(&self) -> WaitService<Backend> {
        WaitService {
            session: self.session.clone(),
            active_work: self.active.lock().unwrap().clone().unwrap(),
        }
    }
    async fn send(&self, text: &str) -> parrot_runtime::agent_turn::lifecycle::SendResult {
        bounded(self.session.send(
            vec![ConversationPart::text(text)],
            id(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        ))
        .await
        .unwrap()
    }
    async fn events(&self) -> Vec<Event> {
        self.history
            .repository()
            .events()
            .replay()
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.agent_session_id == self.history.agent_session_id())
            .collect()
    }
    async fn endings(&self) -> usize {
        self.events()
            .await
            .iter()
            .filter(|event| event.has_turn_ended())
            .count()
    }
    fn active_work(&self) -> Arc<ActiveWorkReminder> {
        self.active.lock().unwrap().clone().unwrap()
    }
}
struct Factory {
    histories: Arc<Histories>,
    resources: UserSessionResources<()>,
    built: Mutex<BTreeMap<String, Arc<Built>>>,
    providers: Mutex<BTreeMap<String, Scripted>>,
}
impl Factory {
    fn get(&self, name: &str) -> Arc<Built> {
        self.built.lock().unwrap().get(name).unwrap().clone()
    }
    fn bind(&self, scope: &Arc<Scope<Security>>) {
        let built = self.get(&scope.identity().session.name);
        built.queues.bind_scope(scope).unwrap();
        let active = Arc::new(ActiveWorkReminder {
            blockers: vec![
                Arc::new(ChildBlocker {
                    children: scope.children().clone(),
                    owner_session_id: scope.identity().session.session_id.clone(),
                }),
                Arc::new(ProcessBlocker {
                    processes: built.processes.clone(),
                }),
                Arc::new(TaskBlocker {
                    tasks: built.tasks.clone(),
                    templates: templates(),
                }),
                Arc::new(QueueBlocker {
                    queues: built.queues.clone(),
                    templates: templates(),
                }),
            ],
            templates: templates(),
        });
        *built.active.lock().unwrap() = Some(active.clone());
        *built.callback.0.lock().unwrap() = Some(Arc::new(SessionCompletion {
            questions: Some(built.questions.clone()),
            active_work: active,
            reminders: built.session.backend().reminders().clone(),
            history: built.history.clone(),
            broker: built.broker.clone(),
            publisher: built.broker.clone() as Arc<Publisher>,
        }));
    }
}
impl ScopeFactory<Security> for Factory {
    fn create(
        &self,
        prepared: PreparedScope<Security>,
    ) -> BoxFuture<'_, ChildResult<ScopeParts<Security>>> {
        Box::pin(async move {
            let identity = prepared.identity.clone();
            let history = prepared.history.clone();
            let broker = Arc::new(EventBroker::new());
            let publisher: Arc<Publisher> = broker.clone();
            let reminders = Arc::new(
                ExitReminders::new(history.clone(), publisher.clone(), templates())
                    .await
                    .unwrap(),
            );
            let callback = Arc::new(CallbackBinding::default());
            let inventory = QueueInventory::new(
                Owner {
                    session_id: identity.session.session_id.clone(),
                    name: identity.session.name.clone(),
                    parent_session_id: identity.session.parent_session_id.clone(),
                    parent_name: identity.parent_name.clone(),
                },
                id(),
            )
            .unwrap();
            let queues = AgentQueues::new(
                identity.clone(),
                &self.resources,
                inventory,
                Arc::new(QueueLog),
            )
            .unwrap();
            queues.initialize().unwrap();
            let question_registry = Arc::new(Questions::default());
            let delivery = Arc::new(QuestionDelivery::default());
            let questions = Arc::new(ChildQuestionCoordinator::new(
                identity.session.session_id.clone(),
                question_registry.clone(),
                delivery.clone(),
                Arc::new(QuestionTemplates),
            ));
            let ancestor_ids = {
                let built = self.built.lock().unwrap();
                let mut parent = identity.session.parent_session_id.clone();
                let mut ancestors = Vec::new();
                while !parent.is_empty() {
                    let ancestor = built
                        .values()
                        .find(|entry| entry.identity.session.session_id == parent)
                        .unwrap();
                    ancestors.push(parent);
                    parent = ancestor.identity.session.parent_session_id.clone();
                }
                ancestors
            };
            let statistics = history
                .repository()
                .events()
                .get_runtime_statistics()
                .await
                .unwrap();
            let ancestors = ancestor_ids
                .into_iter()
                .map(|id| {
                    let statistics = statistics.get_agent_statistics(&id);
                    (id, statistics)
                })
                .collect();
            let backend = RuntimeBackend::new(RuntimeConfig {
                agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                    disable_max_turns: false,
                    ..Default::default()
                },
                identity: identity.session.clone(),
                history: history.clone(),
                broker: broker.clone(),
                publisher,
                selection: prepared.selection.clone(),
                tools: Arc::new(Tools::default()),
                prompt: Box::new(Prompt),
                skills: Arc::new(Skills),
                context: self.histories.controller(history.clone()),
                status: Arc::new(Status),
                completions: vec![callback.clone()],
                templates: templates(),
                reminders,
                output: Arc::new(support::Output::default()),
                activity: Arc::new(Activity),
                parent_completion: prepared.parent_completion,
                ancestors,
                event_id: Arc::new(id),
            })
            .await
            .map_err(|error| ChildError::failure("runtime", error.to_string()))?;
            let session = AgentSession::new(backend, prepared.lifetime);
            let provider = self
                .providers
                .lock()
                .unwrap()
                .remove(&identity.session.name)
                .unwrap();
            let built = Arc::new(Built {
                identity: identity.clone(),
                session: session.clone(),
                provider,
                history,
                queues: queues.clone(),
                broker,
                callback,
                active: Mutex::new(None),
                tasks: Arc::new(Tasks::default()),
                processes: Arc::new(Processes::default()),
                question_registry,
                questions,
                delivery,
            });
            let adapter = Arc::new(SessionAdapter::new(
                identity.clone(),
                prepared.selection,
                session,
            ));
            self.built
                .lock()
                .unwrap()
                .insert(identity.session.name.clone(), built);
            Ok(ScopeParts {
                session: adapter,
                services: vec![ServiceRegistration::new(queues.clone())],
                publishers: vec![],
                work: vec![],
                resources: vec![],
                final_resources: vec![queues],
            })
        })
    }
}
struct Harness {
    histories: Arc<Histories>,
    factory: Arc<Factory>,
    registry: Arc<AgentRegistry<Security>>,
}
impl Harness {
    fn new() -> Self {
        let histories = Histories::new();
        let root = histories
            .repository
            .events()
            .database()
            .selected_root()
            .clone();
        let resources = UserSessionResources::new(
            root,
            UserSessionId::parse(histories.path.file_name().unwrap().to_str().unwrap()).unwrap(),
            (),
        )
        .unwrap();
        let factory = Arc::new(Factory {
            histories: histories.clone(),
            resources,
            built: Mutex::new(BTreeMap::new()),
            providers: Mutex::new(BTreeMap::new()),
        });
        let registry = AgentRegistry::new(RegistryConfig {
            agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
                disable_recursion_limit: false,
                ..Default::default()
            },
            profiles: Arc::new(support::Profiles),
            factory: factory.clone(),
            histories: histories.clone(),
            ids: Arc::new(support::Ids),
            diagnostics: Arc::new(support::Diagnostics::default()),
            retained: RetainedAgentBudget::new(32),
            lifetime: Arc::new(Signal::new()),
            validate_child: Arc::new(QueueChildAdmissionValidator::validate),
        });
        Self {
            histories,
            factory,
            registry,
        }
    }
    async fn scope(
        &self,
        parent: Option<&Arc<Scope<Security>>>,
        name: &str,
    ) -> Arc<Scope<Security>> {
        self.scope_with_delivery(parent, name, CompletionDelivery::RetainedOnly)
            .await
    }
    async fn scope_with_delivery(
        &self,
        parent: Option<&Arc<Scope<Security>>>,
        name: &str,
        delivery: CompletionDelivery,
    ) -> Arc<Scope<Security>> {
        let identity = if let Some(parent) = parent {
            let mut name_path = parent.identity().name_path.clone();
            name_path.push(name.into());
            Arc::new(AgentIdentity {
                session: SessionIdentity {
                    session_id: id(),
                    name: name.into(),
                    parent_session_id: parent.identity().session.session_id.clone(),
                    depth: parent.identity().session.depth + 1,
                },
                parent_name: parent.identity().session.name.clone(),
                name_path,
                prompt_scope: parent.identity().prompt_scope.derive_child(
                    name,
                    parent.identity().session.depth + 1,
                    "T52 fixture",
                ),
                templates: templates(),
            })
        } else {
            AgentIdentity::root(id(), name.into(), templates())
        };
        let provider = Scripted::default();
        let lineage = PolicyLineage::root();
        let selection = Arc::new(SelectionCell::new(
            support::selection(&provider, "first", "build", 0xff),
            lineage.clone(),
        ));
        let history = self.histories.history(&identity);
        self.factory
            .providers
            .lock()
            .unwrap()
            .insert(name.into(), provider);
        let parts = self
            .factory
            .create(PreparedScope {
                identity: identity.clone(),
                history: history.clone(),
                selection: selection.clone(),
                lineage,
                parent_completion: if delivery == CompletionDelivery::Automatic {
                    ParentScope::completion(
                        identity.clone(),
                        parent,
                        delivery,
                        Arc::new(support::Ids),
                    )
                } else {
                    Arc::new(NoParent)
                },
                lifetime: self.registry.lifetime(),
            })
            .await
            .unwrap();
        let scope = bounded(
            Scope::create(ScopeConfig {
                authority: self.registry.clone(),
                identity,
                history,
                selection,
                parent: parent.cloned(),
                delivery,
                retention: None,
                parts,
            })
            .join(),
        )
        .await
        .unwrap();
        self.factory.bind(&scope);
        if let Some(parent) = parent {
            assert!(parent.children().try_add(scope.clone()).unwrap());
        } else {
            self.registry.register_root(scope.clone()).unwrap();
        }
        scope
    }
    async fn close(&self) {
        bounded(self.registry.shutdown().join()).await.unwrap();
    }
}

fn push(queues: &AgentQueues<Security>, name: &str, populated: bool, closed: bool) {
    queues.create(name, "owned work").unwrap();
    queues
        .push(
            name,
            &if populated {
                vec!["pending".into()]
            } else {
                vec![]
            },
            Direction::Back,
            closed,
            &NeverCancelled,
        )
        .unwrap();
}
fn task(state: TaskState) -> Task {
    Task {
        name: "hidden-owned".into(),
        dependencies: vec![],
        description: "Resolve owned work".into(),
        payload: TaskPayload::Instruction("work".into()),
        acceptance_criteria: "done".into(),
        model: None,
        hidden: true,
        state,
        result: None,
        failure: None,
    }
}
async fn assert_pending<F: Future>(mut future: std::pin::Pin<&mut F>) {
    assert!(std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx).is_pending())).await);
}

#[tokio::test]
async fn queue_blocker_formats_sorted_structured_queues_and_omits_empty_queues() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let built = h.factory.get("root");
    let blocker = QueueBlocker {
        queues: built.queues.clone(),
        templates: super::templates(),
    };
    assert!(blocker.observe().await.unwrap().is_none());
    push(&built.queues, "empty", false, false);
    assert!(blocker.observe().await.unwrap().is_none());
    push(&built.queues, "zulu", true, true);
    push(&built.queues, "alpha", true, false);
    built
        .queues
        .push(
            "alpha",
            &["second".into()],
            Direction::Back,
            false,
            &NeverCancelled,
        )
        .unwrap();
    let observation = blocker.observe().await.unwrap().unwrap();
    assert_eq!(observation.work_section, None);
    assert_eq!(
        observation.reminder.as_deref(),
        Some(
            "This turn must not exit while queues owned by this agent contain items.\n- alpha (remaining items: 2)\n- zulu (remaining items: 1)\nDrain every listed queue before finishing, even when asked to stop. Use queue_take to consume items or coordinate consumers and wait until all owned queues are empty. Closing a queue does not drain it. There is no waiver or abandon action. Parent-owned queues do not block completion; an empty open queue does not block completion."
        )
    );
    for name in ["alpha", "zulu"] {
        built.queues.try_take(name, 2, Direction::Front).unwrap();
    }
    assert!(blocker.observe().await.unwrap().is_none());
    h.close().await;
}

#[tokio::test]
async fn owned_queues_block_only_when_populated_for_all_four_closed_empty_combinations() {
    for closed in [false, true] {
        for populated in [false, true] {
            let h = Harness::new();
            let _scope = h.scope(None, "root").await;
            let built = h.factory.get("root");
            push(&built.queues, "owned-work", populated, closed);
            let subscription = built.broker.subscribe();
            let gates = built.gates(if populated { 2 } else { 1 });
            let sent = built.send("Stop now").await;
            gates[0].entered().await;
            gates[0].open();
            if populated {
                gates[1].entered().await;
                assert_eq!(built.endings().await, 0);
                assert!(!sent.execution.is_finished());
                {
                    let requests = built.provider.requests.lock().unwrap();
                    assert_eq!(requests.len(), 2);
                    assert!(requests[1].messages.iter().any(|message| {
                        message.role == sessions::Role::System
                            && message
                                .content()
                                .contains("owned-work (remaining items: 1)")
                            && message.content().contains("even when asked to stop")
                    }));
                    assert!(
                        !requests[1]
                            .messages
                            .iter()
                            .any(|message| message.role == sessions::Role::Assistant)
                    );
                }
                assert!(
                    built
                        .events()
                        .await
                        .iter()
                        .all(|event| !event.has_agent_notice()
                            || event.agent_notice().kind != "active_work_reminder")
                );
                built
                    .queues
                    .try_take("owned-work", 1, Direction::Front)
                    .unwrap();
                gates[1].open();
            }
            assert_eq!(
                bounded(sent.execution.join()).await.unwrap().status,
                ExecutionStatus::Succeeded
            );
            assert_eq!(built.endings().await, 1);
            assert_eq!(
                built.provider.requests.lock().unwrap().len(),
                if populated { 2 } else { 1 }
            );
            let mut notices = 0;
            while let Some(event) = subscription.try_next() {
                if event.has_agent_notice() && event.agent_notice().kind == "active_work_reminder" {
                    notices += 1;
                }
            }
            assert_eq!(notices, usize::from(populated));
            let conversation = built.history.conversation().await.unwrap();
            assert_eq!(
                conversation
                    .iter()
                    .filter(|entry| entry.message.role == MessageRole::Assistant)
                    .count(),
                1
            );
            let jsonl = std::fs::read_to_string(
                h.histories
                    .path
                    .join(format!("{}.jsonl", built.history.agent_session_id())),
            )
            .unwrap();
            assert!(jsonl.contains("answer"));
            h.close().await;
        }
    }
}

#[tokio::test]
async fn failed_child_turn_failed_append_does_not_leave_a_completion_blocker() {
    let h = Harness::new();
    let parent = h.scope(None, "root").await;
    let child_scope = h.scope(Some(&parent), "worker").await;
    let child = h.factory.get("worker");
    let gate = child.gates(1).remove(0);
    child
        .provider
        .failures
        .lock()
        .unwrap()
        .push_back(sessions::Error::Provider(
            "child-provider-failure-marker".into(),
        ));
    h.histories.repository.events().transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_child_turn_failed BEFORE INSERT ON event WHEN instr(NEW.payload, 'child-provider-failure-marker') > 0 BEGIN SELECT RAISE(ABORT, 'rejected TurnFailed append'); END;")?;
        Ok(())
    }).await.unwrap();
    let done = child
        .session
        .enqueue_execution(
            vec![ConversationPart::text("child work")],
            id(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    gate.entered().await;
    let blocker = ChildBlocker {
        children: parent.children().clone(),
        owner_session_id: parent.identity().session.session_id.clone(),
    };
    assert!(blocker.observe().await.unwrap().is_some());
    gate.open();
    assert_eq!(
        bounded(done.join()).await.unwrap().status,
        ExecutionStatus::Failed
    );
    let events = child.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_started())
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_turn_failed())
            .count(),
        0
    );
    assert!(!child_scope.session().is_active());
    assert_eq!(blocker.observe().await.unwrap(), None);
    let root = h.factory.get("root");
    let gate = root.gates(1).remove(0);
    let sent = root.send("finish").await;
    gate.entered().await;
    gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.endings().await, 1);
    h.close().await;
}

#[tokio::test]
async fn active_direct_child_blocks_until_real_child_execution_settles() {
    let h = Harness::new();
    let parent = h.scope(None, "root").await;
    let child_scope = h.scope(Some(&parent), "worker").await;
    let child = h.factory.get("worker");
    let child_gate = child.gates(1).remove(0);
    let child_sent = child.send("child work").await;
    child_gate.entered().await;
    let root = h.factory.get("root");
    let gates = root.gates(2);
    let sent = root.send("finish").await;
    gates[0].entered().await;
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(root.endings().await, 0);
    assert!(
        root.provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| message.role == sessions::Role::System
                && message
                    .content()
                    .contains(&child_scope.identity().session.session_id))
    );
    child_gate.open();
    assert_eq!(
        bounded(child_sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert!(!child_scope.session().is_active());
    assert!(
        ChildBlocker {
            children: parent.children().clone(),
            owner_session_id: parent.identity().session.session_id.clone()
        }
        .observe()
        .await
        .unwrap()
        .is_none()
    );
    gates[1].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.endings().await, 1);
    assert_eq!(root.provider.requests.lock().unwrap().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn automatic_direct_child_completion_restarts_parent_once_with_one_active_work_reminder() {
    let h = Harness::new();
    let parent = h.scope(None, "root").await;
    let child_scope = h
        .scope_with_delivery(Some(&parent), "worker", CompletionDelivery::Automatic)
        .await;
    let child = h.factory.get("worker");
    let child_gate = child.gates(1).remove(0);
    let child_done = child
        .session
        .enqueue_execution(
            vec![ConversationPart::text("child work")],
            id(),
            None,
            None,
            Arc::new(NeverCancelled),
        )
        .unwrap();
    child_gate.entered().await;
    let root = h.factory.get("root");
    let subscription = root.broker.subscribe();
    let gates = root.gates(3);
    let sent = root.send("finish").await;
    gates[0].entered().await;
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(root.endings().await, 0);
    child_gate.open();
    bounded(child_done.join()).await.unwrap();
    gates[1].open();
    gates[2].entered().await;
    gates[2].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.provider.requests.lock().unwrap().len(), 3);
    assert_eq!(root.endings().await, 2);
    let mut notices = 0;
    while let Some(event) = subscription.try_next() {
        if event.has_agent_notice() && event.agent_notice().kind == "active_work_reminder" {
            notices += 1;
        }
    }
    assert_eq!(notices, 1);
    {
        let requests = root.provider.requests.lock().unwrap();
        assert!(requests[2].messages.iter().any(|message| {
            message.role == sessions::Role::System
                && message
                    .content()
                    .contains(&child_scope.identity().session.session_id)
        }));
    }
    assert_eq!(
        root.history
            .conversation()
            .await
            .unwrap()
            .iter()
            .filter(|entry| entry.message.role == MessageRole::Assistant)
            .count(),
        2
    );
    h.close().await;
}

#[tokio::test]
async fn scope_excludes_active_sibling_grandchild_parent_queue_and_foreign_process_owner() {
    let h = Harness::new();
    let root_scope = h.scope(None, "root").await;
    let monitored_scope = h.scope(Some(&root_scope), "monitored").await;
    let sibling_scope = h.scope(Some(&root_scope), "sibling").await;
    let idle_child = h.scope(Some(&monitored_scope), "idle-child").await;
    let grandchild_scope = h.scope(Some(&idle_child), "grandchild").await;
    let root = h.factory.get("root");
    push(&root.queues, "parent-owned", true, false);
    root.processes
        .0
        .lock()
        .unwrap()
        .push(ActiveWorkObservation {
            id: "root/foreign".into(),
            name: "foreign".into(),
            state: ActiveWorkState::Running,
        });
    let sibling = h.factory.get("sibling");
    let grandchild = h.factory.get("grandchild");
    let sibling_gate = sibling.gates(1).remove(0);
    let grandchild_gate = grandchild.gates(1).remove(0);
    let sibling_sent = sibling.send("held sibling").await;
    let grandchild_sent = grandchild.send("held grandchild").await;
    sibling_gate.entered().await;
    grandchild_gate.entered().await;
    assert!(sibling_scope.session().is_active());
    assert!(grandchild_scope.session().is_active());
    assert!(!idle_child.session().is_active());
    let monitored = h.factory.get("monitored");
    let subscription = monitored.broker.subscribe();
    let sent = monitored.send("finish independently").await;
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(monitored.endings().await, 1);
    assert_eq!(monitored.provider.requests.lock().unwrap().len(), 1);
    while let Some(event) = subscription.try_next() {
        assert!(!event.has_agent_notice() || event.agent_notice().kind != "active_work_reminder");
    }
    assert_eq!(root.queues.get("parent-owned").unwrap().size, 1);
    assert_eq!(root.processes.active().len(), 1);
    sibling_gate.open();
    grandchild_gate.open();
    bounded(sibling_sent.execution.join()).await.unwrap();
    bounded(grandchild_sent.execution.join()).await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn hidden_pending_running_failed_tasks_require_explicit_resolution_before_terminal() {
    for state in [TaskState::Pending, TaskState::Running, TaskState::Failed] {
        let h = Harness::new();
        let _scope = h.scope(None, "root").await;
        let root = h.factory.get("root");
        *root.tasks.values.lock().unwrap() = vec![task(state)];
        let gates = root.gates(2);
        let sent = root.send("finish").await;
        gates[0].entered().await;
        gates[0].open();
        gates[1].entered().await;
        assert_eq!(root.endings().await, 0);
        assert!(!sent.execution.is_finished());
        let expected = format!(
            "hidden-owned [{}]",
            match state {
                TaskState::Pending => "pending",
                TaskState::Running => "running",
                TaskState::Failed => "failed",
                _ => unreachable!(),
            }
        );
        assert!(
            root.provider.requests.lock().unwrap()[1]
                .messages
                .iter()
                .any(|message| message.content().contains(&expected))
        );
        assert_eq!(root.tasks.values.lock().unwrap()[0].state, state);
        root.tasks.values.lock().unwrap()[0].state = TaskState::Succeeded;
        gates[1].open();
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
        assert_eq!(root.endings().await, 1);
        assert_eq!(root.provider.requests.lock().unwrap().len(), 2);
        h.close().await;
    }
}

#[tokio::test]
async fn local_domain_process_observation_blocks_until_owner_reports_resolution() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    root.processes
        .0
        .lock()
        .unwrap()
        .push(ActiveWorkObservation {
            id: "root/compiler".into(),
            name: "compiler".into(),
            state: ActiveWorkState::Running,
        });
    let gates = root.gates(2);
    let sent = root.send("finish").await;
    gates[0].entered().await;
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(root.endings().await, 0);
    assert!(
        root.provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| message
                .content()
                .contains("Running processes:\n- root/compiler (name: compiler)"))
    );
    assert_eq!(root.processes.active().len(), 1);
    root.processes.0.lock().unwrap().clear();
    gates[1].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.endings().await, 1);
    h.close().await;
}

#[tokio::test]
async fn reminder_changed_after_construction_retains_candidate_and_clearing_permits_one_terminal() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    let gates = root.gates(2);
    let sent = root.send("finish").await;
    gates[0].entered().await;
    root.session
        .set_exit_reminder(
            "goal".into(),
            "changed after construction".into(),
            Arc::new(NeverCancelled),
        )
        .unwrap()
        .join()
        .await
        .unwrap();
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(root.endings().await, 0);
    assert!(
        root.provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| message.role == sessions::Role::System
                && message.content().contains("changed after construction"))
    );
    assert_eq!(
        root.provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .filter(|message| message.role == sessions::Role::Assistant)
            .count(),
        1
    );
    assert_eq!(root.events().await.iter().filter(|event| event.has_agent_notice() && event.agent_notice().kind == "exit_reminder").count(), 1);
    root.session
        .clear_exit_reminder("goal".into(), Arc::new(NeverCancelled))
        .unwrap()
        .join()
        .await
        .unwrap();
    gates[1].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.endings().await, 1);
    assert_eq!(
        root.history
            .conversation()
            .await
            .unwrap()
            .iter()
            .filter(|entry| entry.message.role == MessageRole::Assistant)
            .count(),
        2
    );
    assert!(root.session.backend().reminders().titles().await.is_empty());
    h.close().await;
}

#[tokio::test]
async fn pending_child_questions_precede_active_work_and_exit_reminders_and_retain_candidate() {
    let h = Harness::new();
    let scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    push(&root.queues, "remaining", true, false);
    root.session
        .backend()
        .reminders()
        .set("goal", "do not finish", &NeverCancelled)
        .await
        .unwrap();
    let mut asks = Vec::new();
    for name in ["zeta", "alpha"] {
        let identity = Arc::new(ChildIdentity {
            session_id: id(),
            name: name.into(),
            parent_session_id: scope.identity().session.session_id.clone(),
        });
        root.question_registry
            .0
            .lock()
            .unwrap()
            .push(Arc::new(ChildQuestion::new(identity.clone())));
        let coordinator = root.questions.clone();
        asks.push(tokio::spawn(async move {
            coordinator
                .ask(
                    identity,
                    vec![QuestionDefinition {
                        header: "Decision".into(),
                        prompt: "Choose".into(),
                        options: vec![],
                        multiple: false,
                        custom: true,
                    }],
                    &NeverCancelled,
                )
                .await
        }));
    }
    bounded(async {
        while root.questions.pending().len() != 2 || root.delivery.0.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let gates = root.gates(2);
    let subscription = root.broker.subscribe();
    let sent = root.send("finish").await;
    gates[0].entered().await;
    gates[0].open();
    gates[1].entered().await;
    assert_eq!(root.endings().await, 0);
    assert_eq!(root.tasks.observations.load(Ordering::SeqCst), 0);
    {
        let requests = root.provider.requests.lock().unwrap();
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == sessions::Role::System
                    && message.content()
                        == "Pending child questions:\n- alpha\n- zeta\nReply before finishing.")
        );
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .filter(|message| message.role == sessions::Role::Assistant)
                .count(),
            1
        );
    }
    let events = root.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_agent_notice()
                && event.agent_notice().kind == "pending_child_question_reminder")
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| event.has_agent_notice() && event.agent_notice().kind == "exit_reminder").count(), 0);
    while let Some(event) = subscription.try_next() {
        assert!(!event.has_agent_notice() || event.agent_notice().kind != "active_work_reminder");
    }
    root.questions
        .reply("alpha", QuestionReply::answered(vec!["yes".into()]))
        .unwrap();
    root.questions
        .reply("zeta", QuestionReply::answered(vec!["yes".into()]))
        .unwrap();
    for ask in asks {
        assert_eq!(bounded(ask).await.unwrap().unwrap().answers, ["yes"]);
    }
    root.queues
        .try_take("remaining", 1, Direction::Front)
        .unwrap();
    root.session
        .backend()
        .reminders()
        .clear("goal", &NeverCancelled)
        .await
        .unwrap();
    gates[1].open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    assert_eq!(root.endings().await, 1);
    assert_eq!(root.provider.requests.lock().unwrap().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn wait_wakes_on_incoming_agent_task_process_question_and_ordinary_causes() {
    for cause in [
        Some("researcher completion"),
        Some("AgentTask graph graph-call completion"),
        Some("process compiler completion"),
        Some("child question"),
        None,
    ] {
        let h = Harness::new();
        let _scope = h.scope(None, "root").await;
        let root = h.factory.get("root");
        let service = root.wait_service();
        let mut waiting =
            Box::pin(service.wait(WaitDuration::new(10_000).unwrap(), &NeverCancelled));
        assert_pending(waiting.as_mut()).await;
        let sent = root
            .session
            .send(
                vec![ConversationPart::text("wakeup")],
                id(),
                protocol::Delivery::DELIVERY_STEER,
                IncomingActivity {
                    name: "notification".into(),
                    cause: cause.map(str::to_owned),
                    ..IncomingActivity::default()
                },
                Arc::new(NeverCancelled),
            )
            .await
            .unwrap();
        assert_eq!(
            bounded(waiting).await.unwrap(),
            cause.map_or_else(
                || "wait interrupted".into(),
                |cause| format!("wait interrupted due to {cause}")
            )
        );
        bounded(sent.execution.join()).await.unwrap();
        h.close().await;
    }
}

#[tokio::test]
async fn wait_observes_already_pending_durable_input_without_starting_a_provider() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    root.session
        .record(
            vec![ConversationPart::text("already here")],
            id(),
            protocol::Delivery::DELIVERY_STEER,
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(
            root.wait_service()
                .wait(WaitDuration::new(10_000).unwrap(), &NeverCancelled)
        )
        .await
        .unwrap(),
        "wait interrupted"
    );
    assert!(root.provider.requests.lock().unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn cancelled_and_dropped_waits_release_registration_without_erasing_blockers() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    push(&root.queues, "remaining", true, true);
    *root.tasks.values.lock().unwrap() = vec![task(TaskState::Failed)];
    root.session
        .backend()
        .reminders()
        .set("goal", "still needed", &NeverCancelled)
        .await
        .unwrap();
    let service = root.wait_service();
    let cancel = Signal::new();
    let mut waiting = Box::pin(service.wait(WaitDuration::new(10_000).unwrap(), &cancel));
    assert_pending(waiting.as_mut()).await;
    cancel.cancel();
    assert!(matches!(bounded(waiting).await, Err(TurnError::Cancelled)));
    let mut dropped = Box::pin(service.wait(WaitDuration::new(10_000).unwrap(), &NeverCancelled));
    assert_pending(dropped.as_mut()).await;
    drop(dropped);
    assert!(
        bounded(
            root.session
                .wait_for_incoming_input(Duration::ZERO, &NeverCancelled)
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(root.queues.get("remaining").unwrap().size, 1);
    assert_eq!(
        root.tasks.values.lock().unwrap()[0].state,
        TaskState::Failed
    );
    assert_eq!(root.session.backend().reminders().titles().await, ["goal"]);
    assert!(
        root.active_work()
            .build()
            .await
            .unwrap()
            .unwrap()
            .contains("hidden-owned [failed]")
    );
    assert!(root.provider.requests.lock().unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn duplicate_input_does_not_wake_wait_but_new_input_wakes_and_promotes_once() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    let gate = root.gates(1).remove(0);
    let message_id = id();
    let sent = root
        .session
        .send(
            vec![ConversationPart::text("first")],
            message_id.clone(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    gate.entered().await;
    let service = root.wait_service();
    let mut waiting = Box::pin(service.wait(WaitDuration::new(10_000).unwrap(), &NeverCancelled));
    assert_pending(waiting.as_mut()).await;
    root.session
        .send(
            vec![ConversationPart::text("first")],
            message_id,
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert_pending(waiting.as_mut()).await;
    root.session
        .send(
            vec![ConversationPart::text("wakeup")],
            id(),
            protocol::Delivery::DELIVERY_STEER,
            IncomingActivity::default(),
            Arc::new(NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(bounded(waiting).await.unwrap(), "wait interrupted");
    gate.open();
    assert_eq!(
        bounded(sent.execution.join()).await.unwrap().status,
        ExecutionStatus::Succeeded
    );
    let events = root.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.has_input_admitted())
            .count(),
        2
    );
    let conversation = root.history.conversation().await.unwrap();
    assert_eq!(
        conversation
            .iter()
            .filter(|entry| entry.message.role == MessageRole::User
                && entry.message.parts == [ConversationPart::text("wakeup")])
            .count(),
        1
    );
    h.close().await;
}

fn candidate(root: &Built) -> CompletionCandidate<Security> {
    CompletionCandidate {
        session_id: root.history.agent_session_id().into(),
        message_id: id(),
        assistant: sessions::Message::assistant("candidate assistant", vec![]),
        selection: support::selection(&root.provider, "first", "build", 0xff),
    }
}
struct RejectPublication {
    history: AgentHistoryView,
    cancel: bool,
    attempts: AtomicUsize,
}
impl EventPublisher for RejectPublication {
    type Event = Event;
    type Error = BrokerError;
    fn publish_committed<'a>(
        &'a self,
        committed: &'a Committed<Event>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            assert!(
                self.history
                    .repository()
                    .events()
                    .replay()
                    .await
                    .unwrap()
                    .contains(committed.value())
            );
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.cancel {
                Err(OperationError::Cancelled)
            } else {
                Err(OperationError::Failed(BrokerError::InvalidCapacity))
            }
        })
    }
    fn publish_transient<'a>(
        &'a self,
        _: Event,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        panic!("retained callback must not publish transiently")
    }
}

#[tokio::test]
async fn retained_callback_publication_failure_or_cancellation_preserves_committed_notice_candidate_and_prompt()
 {
    for cancel in [false, true] {
        let h = Harness::new();
        let _scope = h.scope(None, "root").await;
        let root = h.factory.get("root");
        root.session
            .backend()
            .reminders()
            .set("goal", "still required", &NeverCancelled)
            .await
            .unwrap();
        let publisher = Arc::new(RejectPublication {
            history: root.history.clone(),
            cancel,
            attempts: AtomicUsize::new(0),
        });
        let callback = SessionCompletion {
            questions: Some(root.questions.clone()),
            active_work: root.active_work(),
            reminders: root.session.backend().reminders().clone(),
            history: root.history.clone(),
            broker: root.broker.clone(),
            publisher: publisher.clone() as Arc<Publisher>,
        };
        let result = callback.complete(&candidate(&root), &NeverCancelled).await;
        assert!(match result {
            Err(TurnError::Publication(OperationError::Cancelled)) => cancel,
            Err(TurnError::Publication(OperationError::Failed(BrokerError::InvalidCapacity))) =>
                !cancel,
            _ => false,
        });
        assert_eq!(publisher.attempts.load(Ordering::SeqCst), 1);
        let events = root.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.has_agent_notice()
                    && event.agent_notice().kind == "exit_reminder")
                .count(),
            1
        );
        assert_eq!(root.endings().await, 0);
        let conversation = root.history.conversation().await.unwrap();
        assert_eq!(conversation.len(), 2);
        assert_eq!(conversation[0].message.role, MessageRole::Assistant);
        assert_eq!(
            conversation[0].message.parts,
            [ConversationPart::text("candidate assistant")]
        );
        assert_eq!(conversation[1].message.role, MessageRole::System);
        assert!(
            parrot_storage::events::content(&conversation[1].message.parts)
                .contains("still required")
        );
        assert_eq!(root.session.backend().reminders().titles().await, ["goal"]);
        h.close().await;
    }
}

#[tokio::test]
async fn real_append_failure_prevents_active_and_retained_callback_publication() {
    for retained in [false, true] {
        let h = Harness::new();
        let _scope = h.scope(None, "root").await;
        let root = h.factory.get("root");
        if retained {
            root.session
                .backend()
                .reminders()
                .set("goal", "required", &NeverCancelled)
                .await
                .unwrap();
        } else {
            push(&root.queues, "remaining", true, false);
        }
        let subscription = root.broker.subscribe();
        h.histories
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
        let callback = root.callback.0.lock().unwrap().clone().unwrap();
        assert!(matches!(
            callback.complete(&candidate(&root), &NeverCancelled).await,
            Err(TurnError::Storage(_))
        ));
        assert!(subscription.try_next().is_none());
        assert_eq!(
            root.session.backend().reminders().titles().await,
            if retained { vec!["goal"] } else { vec![] }
        );
        if !retained {
            assert_eq!(root.queues.get("remaining").unwrap().size, 1);
        }
        h.close().await;
    }
}

#[tokio::test]
async fn callback_pre_cancel_precedes_blocker_observation_and_reminder_render_count() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    root.session
        .backend()
        .reminders()
        .set("goal", "required", &NeverCancelled)
        .await
        .unwrap();
    let cancel = Signal::new();
    cancel.cancel();
    let callback = root.callback.0.lock().unwrap().clone().unwrap();
    assert!(matches!(
        callback.complete(&candidate(&root), &cancel).await,
        Err(TurnError::Cancelled)
    ));
    assert_eq!(root.tasks.observations.load(Ordering::SeqCst), 0);
    assert_eq!(root.endings().await, 0);
    let first = root
        .session
        .backend()
        .reminders()
        .build()
        .await
        .unwrap()
        .unwrap();
    let expected = templates()
        .render(
            "agent-session.exit-reminder",
            &[("reminders".into(), "- goal: required".into())],
            &NeverCancelled,
        )
        .unwrap();
    assert_eq!(first, expected);
    h.close().await;
}

#[tokio::test]
async fn actual_wait_timeout_includes_work_sections_but_not_queue_only_reminders() {
    let h = Harness::new();
    let _scope = h.scope(None, "root").await;
    let root = h.factory.get("root");
    push(&root.queues, "remaining", true, true);
    root.processes
        .0
        .lock()
        .unwrap()
        .push(ActiveWorkObservation {
            id: "root/compiler".into(),
            name: "compiler".into(),
            state: ActiveWorkState::Running,
        });
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        root.wait_service()
            .wait(WaitDuration::new(10_000).unwrap(), &NeverCancelled),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        result,
        "Wait timed out after 10000 ms.\nRunning processes:\n- root/compiler (name: compiler)"
    );
    assert_eq!(root.queues.get("remaining").unwrap().size, 1);
    assert_eq!(root.processes.active().len(), 1);
    h.close().await;
}
