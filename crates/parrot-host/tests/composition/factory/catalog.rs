//! Catalog adapter tests. Fixture family services are synthetic, NOT production host/security proof.
//! The real retained families/catalog are assembled; fixture data is adapted from frozen
//! 4674a72 tests/catalog/{support.rs,binding.rs}, SHA256 cf1f4d66d8ef0311d888d8f779ab1dd8c03c3dbcd26a0341a0a26925d927dc98 / 957d1d320266733bfb89aa8a045883f5ad885862f1b3e60bffc76fc49750b7e9.
use super::*;

#[allow(dead_code)]
mod fixture_support {
    use super::{ContextSelection, SharedSecurity};
    // Deterministic catalog-binding fixtures, not filesystem policy or host adapters.

    use std::{
        collections::{BTreeMap, VecDeque},
        io,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{Instant, SystemTime},
    };

    use parrot_domain::{
        contracts::{
            async_ops::{
                BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult,
            },
            filesystem::{
                AuthorizedMutationService, AuthorizedReadService, BoundRead, EntryKind,
                FileMetadata, FileReadError, FileReadErrorKind, LinkPolicy, MutationRead,
                MutationRequest, MutationTarget, ReadRequest,
            },
            interactions::{
                ChildQuestionAnswers, QuestionDefinition, QuestionReply, QuestionRequester,
            },
            paths::{ResolvedRead, RootOrigin, SelectedSessionRoot},
            runtime::{AgentQueues, AgentRuntime, TaskController},
            security::AccessSnapshot,
        },
        values::{
            activity::{AgentSessionActivitySnapshot, DrainState},
            agent_tasks::Task,
            context::{ContextSize, ContextSnapshot},
        },
    };
    use parrot_foundation::template_catalog::PromptTemplateCatalog;
    use parrot_provider::image_generation::{ImageGenerationRequest, ImageGenerationResult};
    use parrot_storage::{
        artifacts::{ImageArtifactRepository, ImageArtifactStore},
        database::SessionDatabase,
    };
    use parrot_tools::{
        agents::{
            AgentToolError, ModelSelector, NamedRequest, SendRequest, SendResult, SpawnRequest,
            SpawnResult, input::WaitInput, status::StatusSnapshot,
        },
        process::{
            ownership::{
                domain::CapturedProcessLauncher,
                execution::{Execution, ProcessLifetime},
                managed::{CompletionMessage, CompletionSink, NoProcessDiagnostics},
                monitor::{MonitorEvent, MonitorIdentity, MonitorReport, MonitorSink},
                owner::{ProcessAgent, ShellProcessOwner},
            },
            tools::{
                CommandRequest, ProcessResolver, ProcessToolServices, ProcessWorkspace,
                ResolvedProcess,
            },
        },
        session::{
            Compaction, Failure, ImageProvider, PermissionReply, PermissionServices,
            SessionServices, WriteTarget, WriteTargetKind,
        },
        tasks_queues::{
            FailureKind, ToolFailure,
            queues::{
                CreateRequest, Direction, InfoRequest, PushRequest, QueueDiagnostic,
                QueueDiagnostics, QueueInfo, QueueTarget, QueueToolBackend, ResolvedQueue,
                TakeRequest, TakeResult,
            },
            tasks::{TaskDetail, TaskQuery, TaskSnapshot, TaskUpdate},
        },
    };

    pub const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0, 5, 0, 1, 0xff, 0x89, 0x99,
        0x3d, 0x1d, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct Security {
        pub id: i32,
    }
    impl AccessSnapshot for Security {
        type Error = String;
        fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
            Err(format!(
                "fixture snapshot {} does not authorize physical reads",
                self.id
            ))
        }
        fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
            Err(format!(
                "fixture snapshot {} does not authorize physical writes",
                self.id
            ))
        }
    }
    pub type Snapshot = SharedSecurity<Security>;
    pub type Selection = ContextSelection<Security>;

    pub struct NoCancel;
    impl Cancellation for NoCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct Call {
        pub operation: &'static str,
        pub detail: String,
        pub security_id: Option<i32>,
        pub selection_id: Option<i32>,
    }
    #[derive(Clone, Debug, Default)]
    pub struct QueueState {
        pub description: String,
        pub items: VecDeque<String>,
        pub closed: bool,
    }
    pub struct State {
        pub calls: Vec<Call>,
        pub files: BTreeMap<PathBuf, Vec<u8>>,
        pub tasks: Vec<Task>,
        pub task_boundary: Option<(i64, String)>,
        pub checkpoints: Vec<(String, i64, String)>,
        pub reminders: BTreeMap<String, String>,
        pub queues: BTreeMap<String, QueueState>,
        pub diagnostics: Vec<QueueDiagnostic>,
        pub launches: Vec<(CommandRequest, i32)>,
        pub now: Instant,
        pub image_bytes: Vec<u8>,
    }
    impl Default for State {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                files: [
                    ("file.txt".into(), b"alpha\nbeta".to_vec()),
                    ("old.txt".into(), b"old".to_vec()),
                    ("pixel.png".into(), PNG.to_vec()),
                ]
                .into(),
                tasks: Vec::new(),
                task_boundary: None,
                checkpoints: Vec::new(),
                reminders: BTreeMap::new(),
                queues: BTreeMap::new(),
                diagnostics: Vec::new(),
                launches: Vec::new(),
                now: Instant::now(),
                image_bytes: PNG.to_vec(),
            }
        }
    }
    #[derive(Clone, Default)]
    pub struct Services {
        pub state: Arc<Mutex<State>>,
    }
    impl Services {
        pub fn calls(&self) -> Vec<Call> {
            self.state.lock().unwrap().calls.clone()
        }
        pub fn record(
            &self,
            operation: &'static str,
            detail: impl Into<String>,
            security_id: Option<i32>,
            selection_id: Option<i32>,
        ) {
            self.state.lock().unwrap().calls.push(Call {
                operation,
                detail: detail.into(),
                security_id,
                selection_id,
            });
        }
        fn target(request: &MutationRequest) -> MutationTarget {
            MutationTarget {
                physical_path: request.requested.clone(),
                display_path: request.requested.to_string_lossy().into(),
            }
        }
        fn queue_key(target: &QueueTarget<String>) -> String {
            match target {
                QueueTarget::Invoking(name) => name.clone(),
                QueueTarget::Local(target) => target.name.clone(),
            }
        }
        fn queue_info(name: &str, queue: &QueueState) -> QueueInfo {
            QueueInfo {
                path: format!("catalog/{name}"),
                name: name.into(),
                description: queue.description.clone(),
                size: i32::try_from(queue.items.len()).unwrap(),
                closed: queue.closed,
            }
        }
    }
    fn cancelled<E>(cancellation: &dyn Cancellation) -> OperationResult<(), E> {
        cancellation.check().map_err(|_| OperationError::Cancelled)
    }
    fn metadata(path: &Path, kind: EntryKind) -> FileMetadata {
        FileMetadata {
            lexical_path: path.into(),
            physical_path: path.into(),
            kind,
            hidden: false,
        }
    }
    fn missing() -> FileReadError {
        FileReadError::new(FileReadErrorKind::NotFound, "fixture file missing")
    }

    impl AuthorizedReadService for Services {
        type Security = Snapshot;
        fn inspect<'a>(
            &'a self,
            path: &'a Path,
            links: LinkPolicy,
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "inspect",
                    format!("{} {links:?}", path.display()),
                    Some(snapshot.0.id),
                    None,
                );
                if path == Path::new(".") || path == Path::new("dir") {
                    return Ok(metadata(path, EntryKind::Directory));
                }
                if self.state.lock().unwrap().files.contains_key(path) {
                    Ok(metadata(path, EntryKind::Regular))
                } else {
                    Err(OperationError::Failed(missing()))
                }
            })
        }
        fn list_directory<'a>(
            &'a self,
            path: &'a Path,
            links: LinkPolicy,
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "list_directory",
                    format!("{} {links:?}", path.display()),
                    Some(snapshot.0.id),
                    None,
                );
                Ok(vec![metadata(Path::new("file.txt"), EntryKind::Regular)])
            })
        }
        fn read<'a>(
            &'a self,
            request: &'a ReadRequest,
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("read", format!("{request:?}"), Some(snapshot.0.id), None);
                let bytes = self
                    .state
                    .lock()
                    .unwrap()
                    .files
                    .get(&request.requested)
                    .cloned()
                    .ok_or_else(|| OperationError::Failed(missing()))?;
                if request
                    .max_bytes
                    .is_some_and(|maximum| bytes.len() > maximum)
                {
                    return Err(OperationError::Failed(FileReadError::new(
                        FileReadErrorKind::TooLarge,
                        "fixture read limit",
                    )));
                }
                Ok(BoundRead {
                    lexical_path: request.requested.clone(),
                    physical_path: request
                        .expected_physical
                        .clone()
                        .unwrap_or_else(|| request.requested.clone()),
                    bytes,
                })
            })
        }
    }
    impl AuthorizedMutationService for Services {
        type Security = Snapshot;
        fn inspect_for_mutation<'a>(
            &'a self,
            request: &'a MutationRequest,
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "mutation_inspect",
                    format!("{request:?}"),
                    Some(snapshot.0.id),
                    None,
                );
                Ok(Self::target(request))
            })
        }
        fn read_for_mutation<'a>(
            &'a self,
            request: &'a MutationRequest,
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "mutation_read",
                    format!("{request:?}"),
                    Some(snapshot.0.id),
                    None,
                );
                let bytes = self
                    .state
                    .lock()
                    .unwrap()
                    .files
                    .get(&request.requested)
                    .cloned();
                if bytes.is_none() && !request.create {
                    return Err(OperationError::Failed(missing()));
                }
                Ok(MutationRead {
                    target: Self::target(request),
                    bytes,
                })
            })
        }
        fn write_for_mutation<'a>(
            &'a self,
            request: &'a MutationRequest,
            bytes: &'a [u8],
            snapshot: &'a Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "mutation_write",
                    format!("{request:?} {bytes:?}"),
                    Some(snapshot.0.id),
                    None,
                );
                self.state
                    .lock()
                    .unwrap()
                    .files
                    .insert(request.requested.clone(), bytes.to_vec());
                Ok(Self::target(request))
            })
        }
    }

    impl ModelSelector for Services {
        fn resolve(&self, selector: &str) -> Result<String, AgentToolError> {
            self.record("model", selector, None, None);
            Ok(selector.into())
        }
    }
    impl AgentRuntime for Services {
        type Security = Snapshot;
        type SpawnRequest = SpawnRequest;
        type SpawnResult = SpawnResult;
        type SendRequest = SendRequest;
        type SendResult = SendResult;
        type InterruptRequest = NamedRequest;
        type InterruptResult = ();
        type StatusRequest = NamedRequest;
        type StatusResult = StatusSnapshot;
        type WaitRequest = WaitInput;
        type WaitResult = String;
        type Error = AgentToolError;
        fn spawn<'a>(
            &'a self,
            request: SpawnRequest,
            security: Snapshot,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<SpawnResult, AgentToolError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("spawn", format!("{request:?}"), Some(security.0.id), None);
                Ok(SpawnResult {
                    name: request.input.name,
                    depth: 1,
                    output_path: "catalog/child/output.txt".into(),
                })
            })
        }
        fn send<'a>(
            &'a self,
            request: SendRequest,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<SendResult, AgentToolError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("send", format!("{request:?}"), None, None);
                Ok(SendResult {
                    name: request.input.name,
                    message_id: "catalog-message".into(),
                    follow_up: false,
                })
            })
        }
        fn interrupt<'a>(
            &'a self,
            request: NamedRequest,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<(), AgentToolError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("interrupt", request.name, None, None);
                Ok(())
            })
        }
        fn status(&self, request: NamedRequest) -> Result<StatusSnapshot, AgentToolError> {
            self.record("agent_status", &request.name, None, None);
            Ok(StatusSnapshot {
                session_id: "catalog-child".into(),
                name: request.name,
                output_path: "catalog/child/output.txt".into(),
                activity: AgentSessionActivitySnapshot {
                    state: DrainState::Idle,
                    current_tool: None,
                    request_session_duration: None,
                    current_provider_request_duration: None,
                    last_provider_request_duration: None,
                    latest_provider_activity_age: None,
                    terminal_outcome: None,
                    recent: Box::new([]),
                },
                children: Vec::new(),
                processes: Vec::new(),
            })
        }
        fn wait<'a>(
            &'a self,
            request: WaitInput,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<String, AgentToolError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("wait", format!("{request:?}"), None, None);
                Ok("wait interrupted".into())
            })
        }
    }
    impl QuestionRequester for Services {
        type Error = AgentToolError;
        fn ask<'a>(
            &'a self,
            questions: Vec<QuestionDefinition>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<QuestionReply, AgentToolError>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("question", format!("{questions:?}"), None, None);
                Ok(QuestionReply::answered(
                    questions.iter().map(|_| "catalog answer".into()).collect(),
                ))
            })
        }
    }
    impl ChildQuestionAnswers for Services {
        type Error = AgentToolError;
        fn reply(&self, agent_name: &str, reply: QuestionReply) -> Result<(), AgentToolError> {
            self.record("answer", format!("{agent_name} {reply:?}"), None, None);
            Ok(())
        }
    }

    pub fn context_snapshot() -> ContextSnapshot {
        ContextSnapshot::new(100, 1000, Some(10), 80)
    }
    impl SessionServices for Services {
        type Security = Snapshot;
        type Selection = Selection;
        fn checkpoint<'a>(
            &'a self,
            title: &'a str,
            assistant_sequence: i64,
            call_id: &'a str,
        ) -> BoxFuture<'a, Result<(), Failure>> {
            Box::pin(async move {
                self.record(
                    "checkpoint",
                    format!("{title} {assistant_sequence} {call_id}"),
                    None,
                    None,
                );
                self.state.lock().unwrap().checkpoints.push((
                    title.into(),
                    assistant_sequence,
                    call_id.into(),
                ));
                Ok(())
            })
        }
        fn set_reminder<'a>(
            &'a self,
            title: &'a str,
            description: &'a str,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<(), Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("set_reminder", format!("{title} {description}"), None, None);
                self.state
                    .lock()
                    .unwrap()
                    .reminders
                    .insert(title.into(), description.into());
                Ok(())
            })
        }
        fn clear_reminder<'a>(
            &'a self,
            title: &'a str,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<bool, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("clear_reminder", title, None, None);
                Ok(self.state.lock().unwrap().reminders.remove(title).is_some())
            })
        }
        fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
            Box::pin(async move {
                self.state
                    .lock()
                    .unwrap()
                    .reminders
                    .keys()
                    .cloned()
                    .collect()
            })
        }
        fn runtime_status<'a>(
            &'a self,
            selection: &'a Selection,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<String, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "runtime_status",
                    "catalog runtime",
                    None,
                    Some(selection.turn.profile.maximum_requests),
                );
                Ok("catalog runtime".into())
            })
        }
        fn statistics_status<'a>(
            &'a self,
            selection: &'a Selection,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<String, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "statistics_status",
                    "catalog statistics",
                    None,
                    Some(selection.turn.profile.maximum_requests),
                );
                Ok("catalog statistics".into())
            })
        }
        fn compact<'a>(
            &'a self,
            selection: &'a Selection,
            target: Option<ContextSize>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "compact",
                    format!("{target:?}"),
                    None,
                    Some(selection.turn.profile.maximum_requests),
                );
                Ok(Compaction {
                    reduced: true,
                    context: context_snapshot(),
                })
            })
        }
        fn estimate_after_tool_result(
            &self,
            selection: &Selection,
            call_id: &str,
            rendered_result: &str,
        ) -> Result<ContextSnapshot, Failure> {
            self.record(
                "estimate",
                format!("{call_id} {rendered_result}"),
                None,
                Some(selection.turn.profile.maximum_requests),
            );
            Ok(context_snapshot())
        }
    }
    impl ImageProvider for Services {
        type Selection = Selection;
        type Security = Snapshot;
        fn generate<'a>(
            &'a self,
            selection: &'a Selection,
            request: ImageGenerationRequest,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<ImageGenerationResult, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "image_generate",
                    format!("{request:?}"),
                    None,
                    Some(selection.turn.profile.maximum_requests),
                );
                Ok(ImageGenerationResult {
                    data: self.state.lock().unwrap().image_bytes.clone(),
                })
            })
        }
    }
    impl PermissionServices for Services {
        type Security = Snapshot;
        type Selection = Selection;
        fn resolve_target(&self, requested: &str) -> Result<WriteTarget, Failure> {
            self.record("permission_target", requested, None, None);
            Ok(WriteTarget {
                path: requested.into(),
                kind: WriteTargetKind::File,
            })
        }
        fn allows_write(&self, snapshot: &Snapshot, target: &WriteTarget) -> bool {
            self.record(
                "permission_allows",
                target.path.display().to_string(),
                Some(snapshot.0.id),
                None,
            );
            false
        }
        fn is_read_only(&self, snapshot: &Snapshot) -> bool {
            self.record("permission_read_only", "false", Some(snapshot.0.id), None);
            false
        }
        fn request<'a>(
            &'a self,
            reason: &'a str,
            targets: Vec<WriteTarget>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<PermissionReply, Failure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "permission_request",
                    format!("{reason} {targets:?}"),
                    None,
                    None,
                );
                Ok(PermissionReply::Granted)
            })
        }
    }
    impl TaskController for Services {
        type Update = TaskUpdate<Selection>;
        type Query = TaskQuery;
        type Snapshot = TaskSnapshot;
        type Error = ToolFailure;
        fn update<'a>(
            &'a self,
            update: TaskUpdate<Selection>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<TaskSnapshot, ToolFailure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record(
                    "task_update",
                    format!("{:?}", update.tasks),
                    None,
                    Some(update.selection.turn.profile.maximum_requests),
                );
                let mut state = self.state.lock().unwrap();
                state.tasks = update.tasks;
                state.task_boundary = Some((
                    update.history_boundary.assistant_sequence,
                    update.history_boundary.call_id,
                ));
                Ok(TaskSnapshot {
                    tasks: state.tasks.clone(),
                    detail: None,
                })
            })
        }
        fn snapshot(&self, query: TaskQuery) -> BoxFuture<'_, Result<TaskSnapshot, ToolFailure>> {
            Box::pin(async move {
                self.record("task_snapshot", format!("{query:?}"), None, None);
                let state = self.state.lock().unwrap();
                let detail = query
                    .name
                    .as_ref()
                    .and_then(|name| state.tasks.iter().find(|task| &task.name == name))
                    .map(|task| TaskDetail {
                        task: task.clone(),
                        agent_name: Some("catalog-worker".into()),
                    });
                Ok(TaskSnapshot {
                    tasks: state.tasks.clone(),
                    detail,
                })
            })
        }
    }
    impl AgentQueues for Services {
        type CreateRequest = CreateRequest;
        type CreateResult = QueueInfo;
        type InfoRequest = InfoRequest<String>;
        type InfoResult = QueueInfo;
        type PushRequest = PushRequest<String>;
        type PushResult = QueueInfo;
        type TakeRequest = TakeRequest<String>;
        type TakeResult = TakeResult;
        type Error = ToolFailure;
        fn create<'a>(
            &'a self,
            request: CreateRequest,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("queue_create", format!("{request:?}"), None, None);
                let mut state = self.state.lock().unwrap();
                let queue =
                    state
                        .queues
                        .entry(request.name.clone())
                        .or_insert_with(|| QueueState {
                            description: request.description,
                            ..QueueState::default()
                        });
                Ok(Self::queue_info(&request.name, queue))
            })
        }
        fn info<'a>(
            &'a self,
            request: InfoRequest<String>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("queue_info", format!("{request:?}"), None, None);
                let name = Self::queue_key(&request.target);
                let state = self.state.lock().unwrap();
                let queue = state.queues.get(&name).ok_or_else(queue_missing)?;
                Ok(Self::queue_info(&name, queue))
            })
        }
        fn push<'a>(
            &'a self,
            request: PushRequest<String>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("queue_push", format!("{request:?}"), None, None);
                let name = Self::queue_key(&request.target);
                let mut state = self.state.lock().unwrap();
                let queue = state.queues.get_mut(&name).ok_or_else(queue_missing)?;
                for item in request.items.into_iter().flatten() {
                    if request.direction == Direction::Front {
                        queue.items.push_front(item);
                    } else {
                        queue.items.push_back(item);
                    }
                }
                queue.closed |= request.close;
                Ok(Self::queue_info(&name, queue))
            })
        }
        fn take<'a>(
            &'a self,
            request: TakeRequest<String>,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<TakeResult, ToolFailure>> {
            Box::pin(async move {
                cancelled(cancellation)?;
                self.record("queue_take", format!("{request:?}"), None, None);
                let name = Self::queue_key(&request.target);
                let mut state = self.state.lock().unwrap();
                let queue = state.queues.get_mut(&name).ok_or_else(queue_missing)?;
                let mut items = Vec::new();
                for _ in 0..request.count {
                    let item = if request.direction == Direction::Back {
                        queue.items.pop_back()
                    } else {
                        queue.items.pop_front()
                    };
                    if let Some(item) = item {
                        items.push(item);
                    } else {
                        break;
                    }
                }
                Ok(TakeResult {
                    acquired: true,
                    info: Some(Self::queue_info(&name, queue)),
                    items,
                })
            })
        }
    }
    fn queue_missing() -> OperationError<ToolFailure> {
        OperationError::Failed(ToolFailure::new(
            FailureKind::Queue,
            "catalog queue missing",
        ))
    }
    impl QueueToolBackend<String> for Services {
        fn resolve(&self, address: &str) -> Result<ResolvedQueue<String>, ToolFailure> {
            self.record("queue_resolve", address, None, None);
            let (owner, name) = address.rsplit_once('/').ok_or_else(|| {
                ToolFailure::new(
                    FailureKind::Registry,
                    "catalog qualified queue missing owner",
                )
            })?;
            Ok(ResolvedQueue {
                owner: owner.into(),
                name: name.into(),
            })
        }
        fn session_id(&self) -> &str {
            "catalog-session"
        }
        fn resolved_session_id<'a>(&self, _: &'a String) -> &'a str {
            "catalog-session"
        }
    }
    impl QueueDiagnostics for Services {
        fn record(&self, event: QueueDiagnostic) {
            self.state.lock().unwrap().diagnostics.push(event);
        }
    }
    impl Clock for Services {
        fn now(&self) -> Instant {
            self.state.lock().unwrap().now
        }
        fn utc_now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
        fn sleep_until<'a>(
            &'a self,
            deadline: Instant,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Cancelled>> {
            Box::pin(async move {
                cancellation.check()?;
                self.record("clock_sleep", format!("{deadline:?}"), None, None);
                self.state.lock().unwrap().now = deadline;
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    pub struct RejectingLauncher {
        pub services: Services,
    }
    impl CapturedProcessLauncher for RejectingLauncher {
        type Request = CommandRequest;
        type Security = Snapshot;
        fn launch(
            &self,
            request: CommandRequest,
            security: Snapshot,
            _: Arc<ProcessLifetime>,
        ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
            Box::pin(async move {
                self.services.record(
                    "process_launch",
                    format!("{request:?}"),
                    Some(security.0.id),
                    None,
                );
                self.services
                    .state
                    .lock()
                    .unwrap()
                    .launches
                    .push((request, security.0.id));
                Err(io::Error::other(
                    "catalog launcher rejected synthetic command",
                ))
            })
        }
    }
    impl ProcessWorkspace for Services {
        fn root(&self) -> &str {
            "/catalog/workspace"
        }
        fn resolves_to_root(&self, target: &str) -> bool {
            target == self.root()
        }
    }
    impl ProcessResolver for Services {
        fn resolve(&self, address: &str) -> io::Result<ResolvedProcess> {
            self.record("process_resolve", address, None, None);
            Err(io::Error::other("catalog process route missing"))
        }
    }
    impl CompletionSink for Services {
        fn send<'a>(
            &'a self,
            message: &'a CompletionMessage,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                cancellation
                    .check()
                    .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
                self.record("process_completion", message.text.clone(), None, None);
                Ok(())
            })
        }
    }
    impl MonitorSink for Services {
        fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String> {
            Ok(format!("{}: {lines}", identity.name))
        }
        fn persist_event<'a>(
            &'a self,
            text: String,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, io::Result<String>> {
            Box::pin(async move {
                self.record("monitor_persist", text, None, None);
                Err(io::Error::other(
                    "catalog fixture does not persist monitor events",
                ))
            })
        }
        fn send_event<'a>(
            &'a self,
            event: MonitorEvent,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                self.record("monitor_event", format!("{event:?}"), None, None);
                Ok(())
            })
        }
        fn observe_failed(&self, identity: &MonitorIdentity, failure: &io::Error) {
            self.record(
                "monitor_failure",
                format!("{} {failure}", identity.name),
                None,
                None,
            );
        }
        fn render_completion(
            &self,
            identity: &MonitorIdentity,
            report: &MonitorReport,
        ) -> io::Result<String> {
            Ok(format!("{}: {report:?}", identity.name))
        }
    }
    impl Services {
        pub fn process_services(
            &self,
            templates: Arc<PromptTemplateCatalog>,
        ) -> Arc<ProcessToolServices<RejectingLauncher>> {
            let owner = Arc::new(
                ShellProcessOwner::new(
                    "catalog-process-owner",
                    None,
                    Arc::new(self.clone()),
                    Arc::new(NoProcessDiagnostics),
                )
                .unwrap(),
            );
            Arc::new(ProcessToolServices {
                owner,
                agent: ProcessAgent {
                    session_id: "catalog-agent".into(),
                    name: "catalog".into(),
                    parent_session_id: String::new(),
                    parent_name: String::new(),
                    depth: 0,
                },
                launcher: Arc::new(RejectingLauncher {
                    services: self.clone(),
                }),
                resolver: Arc::new(self.clone()),
                workspace: Arc::new(self.clone()),
                read_only_prefixes: vec!["printf".into()],
                templates,
                monitor_sink: Arc::new(self.clone()),
            })
        }
    }

    static NEXT_ARTIFACT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    pub struct Artifacts {
        pub repository: Arc<ImageArtifactRepository>,
        pub database: SessionDatabase,
        pub path: PathBuf,
    }
    impl Artifacts {
        pub fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.tmp/t62-catalog-adapter-artifacts")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT_ARTIFACT_FIXTURE.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            let path = std::fs::canonicalize(path).unwrap();
            let root = SelectedSessionRoot::new(path.clone(), RootOrigin::Current).unwrap();
            let database = SessionDatabase::open(root.clone()).unwrap();
            let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
            let repository =
                Arc::new(ImageArtifactRepository::new(database.clone(), store).unwrap());
            Self {
                repository,
                database,
                path,
            }
        }
        pub async fn close(self) {
            self.database.close().await.unwrap();
        }
    }
    impl Default for Artifacts {
        fn default() -> Self {
            Self::new()
        }
    }
}
#[allow(dead_code)]
mod fixture_binding {
    use std::{
        collections::BTreeMap,
        io,
        net::{IpAddr, SocketAddr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::fixture_support::{Artifacts, Selection, Services, Snapshot};
    use parrot_domain::contracts::async_ops::BoxFuture;
    use parrot_foundation::{config::Configuration, template_catalog::PromptTemplateCatalog};
    use parrot_tools::{
        catalog::{self, Catalog},
        files::WorkspacePaths,
        process::ownership::owner::ShellProcessOwner,
        web_fetch::{
            self, FetchError, GzipDecoder, Options, Request, Resolver, Response, ResponseBody,
            Transport, TransportSession, WebFetcher,
        },
    };

    #[derive(Default)]
    pub struct WebState {
        pub sessions: AtomicUsize,
        pub requests: Mutex<Vec<String>>,
    }
    struct NetworkFixture(Arc<WebState>);
    impl Resolver for NetworkFixture {
        fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            Box::pin(async move {
                assert_eq!(host, "catalog.invalid");
                Ok(vec!["8.8.8.8".parse().unwrap()])
            })
        }
    }
    impl Transport for NetworkFixture {
        fn session(&self) -> Result<Box<dyn TransportSession + '_>, FetchError> {
            self.0.sessions.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(NetworkFixture(self.0.clone())))
        }
    }
    impl TransportSession for NetworkFixture {
        fn send<'a>(
            &'a mut self,
            request: &'a Request,
            addresses: &'a [SocketAddr],
            _: &'a Options,
        ) -> BoxFuture<'a, Result<Response, FetchError>> {
            Box::pin(async move {
                assert_eq!(addresses, &["8.8.8.8:443".parse::<SocketAddr>().unwrap()]);
                self.0
                    .requests
                    .lock()
                    .unwrap()
                    .push(request.method.as_str().into());
                Ok(Response {
                    status_code: 200,
                    location: None,
                    content_type: Some("text/plain; charset=utf-8".into()),
                    content_encodings: vec![],
                    body: Box::new(BytesBody(b"catalog web".to_vec())),
                })
            })
        }
    }
    struct BytesBody(Vec<u8>);
    impl ResponseBody for BytesBody {
        fn read<'a>(
            &'a mut self,
            output: &'a mut [u8],
        ) -> BoxFuture<'a, Result<usize, FetchError>> {
            Box::pin(async move {
                let count = output.len().min(self.0.len());
                output[..count].copy_from_slice(&self.0[..count]);
                self.0.drain(..count);
                Ok(count)
            })
        }
    }
    struct NoGzip;
    impl GzipDecoder for NoGzip {
        fn decode(&self, _: Box<dyn ResponseBody>) -> Result<Box<dyn ResponseBody>, FetchError> {
            panic!("plain catalog fixture is not a production gzip decoder")
        }
    }

    pub struct Fixture {
        pub catalog: Catalog<Snapshot, Selection>,
        pub configuration: Configuration,
        pub templates: Arc<PromptTemplateCatalog>,
        pub services: Services,
        pub artifacts: Artifacts,
        pub owner: Arc<ShellProcessOwner>,
        pub web: Arc<WebState>,
    }
    impl Fixture {
        pub fn new(overrides: &str, allow_parent: bool, depth: i32) -> Self {
            let artifacts = Artifacts::new();
            let env = [
                "HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "WORKDIR",
            ]
            .into_iter()
            .map(|name| {
                (
                    name.to_owned(),
                    artifacts.path.join(name).to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
            let config_path = artifacts.path.join("config.yaml");
            std::fs::write(&config_path, overrides).unwrap();
            let (configuration, templates) = PromptTemplateCatalog::load(
                &config_path,
                &artifacts.path.join("predefined.yaml"),
                &env,
            )
            .unwrap();
            let templates = Arc::new(templates);
            let definitions = catalog::configured_definitions(&configuration);
            let services = Services::default();
            let shared = Arc::new(services.clone());
            let process_services = services.process_services(templates.clone());
            let owner = process_services.owner.clone();
            let process = catalog::ProcessFactories::new(process_services, &definitions).unwrap();
            let agents = catalog::AgentFactories::new(
                catalog::AgentServices {
                    runtime: shared.clone(),
                    models: shared.clone(),
                    questions: shared.clone(),
                    answers: shared.clone(),
                    templates: templates.clone(),
                    allow_parent,
                },
                &definitions,
            )
            .unwrap();
            let files = catalog::FileFactories::new(
                services.clone(),
                WorkspacePaths::default(),
                artifacts.repository.clone(),
                configuration.runtime.request_limits.image_bytes_per_read,
                &definitions,
            )
            .unwrap();
            let session = catalog::SessionFactories::new(
                catalog::SessionBindings {
                    control: shared.clone(),
                    images: shared.clone(),
                    permission: shared.clone(),
                    filesystem: shared.clone(),
                    templates: templates.clone(),
                    agent_depth: depth,
                },
                &definitions,
            )
            .unwrap();
            let tasks = catalog::TaskQueueFactories::new(
                catalog::TaskQueueServices {
                    tasks: shared.clone(),
                    filesystem: shared.clone(),
                    templates: templates.clone(),
                    queues: shared.clone(),
                    clock: shared.clone(),
                    diagnostics: shared,
                },
                &definitions,
            )
            .unwrap();
            let web = Arc::new(WebState::default());
            let network = Arc::new(NetworkFixture(web.clone()));
            let fetcher = Arc::new(WebFetcher::new(
                Arc::new(web_fetch::PublicAddressPolicy),
                network.clone(),
                network,
                Arc::new(NoGzip),
                Options::default(),
            ));
            let web_factory = catalog::WebFetchFactory::new(fetcher, &definitions).unwrap();
            let catalog = Catalog::new(process, agents, files, session, tasks, web_factory);
            Self {
                catalog,
                configuration,
                templates,
                services,
                artifacts,
                owner,
                web,
            }
        }
    }
}

use fixture_binding::Fixture;
use fixture_support::{NoCancel, Security};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    values::{config::ModelProfiles, tools::ToolCycleImageBudget},
};
use parrot_provider::{
    catalogs::Model,
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        registry::ProviderRegistry,
    },
    sessions::{self, Provider},
};
use parrot_runtime::{
    agent_turn::services::TurnProfile, compaction::CompactionSettings,
    tool_settlement::ToolSnapshot,
};
use std::collections::BTreeMap;

struct PlanningProvider;
impl Provider for PlanningProvider {
    fn id(&self) -> &str {
        "fixture"
    }
    fn call(
        &self,
        _: sessions::Request,
    ) -> Result<Box<dyn sessions::EventStream>, sessions::Error> {
        panic!("no provider calls in catalog conversion tests")
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, sessions::Error>> {
        panic!("no credential access")
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, sessions::Error>> {
        panic!("no model discovery")
    }
    fn open_session(&self) -> Result<Arc<dyn sessions::Session>, sessions::Error> {
        panic!("no provider session")
    }
}
fn selected(id: i32) -> TurnSelection<Security> {
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(PlanningProvider)],
            BTreeMap::from([("fixture".into(), vec![Model::new("model", "fixture")])]),
        )
        .unwrap(),
    );
    let aliases = Arc::new(ModelAliasCatalog::new(registry.clone(), vec![]).unwrap());
    let routing = Arc::new(ModelRouting::new(aliases, "fixture/model"));
    TurnSelection {
        model: ModelRouter::new(registry, routing).resolve("").unwrap(),
        profile: TurnProfile {
            id: "build".into(),
            maximum_requests: id,
            allowed_tools: None,
            disabled_tools: vec![],
        },
        security: Arc::new(Security { id: 71 }),
    }
}
fn planner() -> Arc<CompactionPlanner> {
    Arc::new(CompactionPlanner::new(
        CompactionSettings {
            trigger_percent: 80,
            target_percent: 50,
            maximum_input_tokens: 0,
            summary_output_tokens: 100,
            summary_instructions: "summary".into(),
            summary_prefix: "summary:".into(),
        },
        ModelProfiles::new(&BTreeMap::new()),
    ))
}

#[tokio::test]
async fn all_real_catalog_handles_order_schemas_support_and_aliases_survive_materialization() {
    let fixture = Fixture::new(
        "tools:\n  read:\n    description: Configured read.\n    parameters:\n      properties:\n        path:\n          type: integer\n          minimum: 7\n",
        false,
        1,
    );
    let Fixture {
        catalog,
        configuration,
        templates,
        services,
        artifacts,
        owner,
        ..
    } = fixture;
    let originals = catalog
        .factories()
        .iter()
        .map(|factory| factory.create())
        .collect::<Vec<_>>();
    let counts = originals.iter().map(Arc::strong_count).collect::<Vec<_>>();
    let adapter = CatalogAdapter::new(catalog, planner(), templates).unwrap();
    for (original, count) in originals.iter().zip(counts) {
        assert_eq!(Arc::strong_count(original), count + 1);
    }
    let first = adapter.materialize().unwrap();
    let second = adapter.materialize().unwrap();
    assert_eq!(
        first
            .tools
            .iter()
            .map(|tool| tool.definition().name.as_str())
            .collect::<Vec<_>>(),
        parrot_tools::catalog::BUILTIN_NAMES
    );
    assert_eq!(first.tools.len(), 30);
    assert_eq!(first.supported.iter().filter(|value| **value).count(), 29);
    for (index, (left, right)) in first.tools.iter().zip(&second.tools).enumerate() {
        match (left, right) {
            (ToolHandle::Ordinary(left), ToolHandle::Ordinary(right)) => {
                assert!(Arc::ptr_eq(left, right))
            }
            (ToolHandle::Borrowed(left), ToolHandle::Borrowed(right)) => {
                assert!(matches!(index, 23 | 24));
                assert!(Arc::ptr_eq(left, right));
            }
            _ => panic!("materialization changed handle kind"),
        }
        assert!(Arc::ptr_eq(
            &originals[index],
            &adapter.catalog().factories()[index].create()
        ));
        assert_eq!(
            first.definitions[index],
            *adapter.catalog().factories()[index].definition()
        );
    }
    assert!(matches!(first.tools[23], ToolHandle::Borrowed(_)));
    assert!(matches!(first.tools[24], ToolHandle::Borrowed(_)));
    let snapshot = ToolSnapshot::document(first).unwrap();
    assert!(snapshot.find("request_write_permission").is_none());
    assert_eq!(
        snapshot.find("bash").unwrap().definition().name,
        "exec_command"
    );
    assert!(snapshot.find("Bash").is_none());
    let definitions = snapshot.definitions();
    let read = definitions
        .iter()
        .find(|definition| definition.name == "read")
        .unwrap();
    assert_eq!(read.description, "Configured read.");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&read.parameters_json).unwrap()["properties"]["path"]
            ["minimum"],
        7
    );
    assert_eq!(
        read.parameters_json,
        configuration.runtime.tools["read"].parameters_json
    );
    assert!(
        snapshot
            .enabled_after_interruption()
            .find("agent_spawn")
            .is_none()
    );
    assert!(snapshot.enabled_after_interruption().find("wait").is_some());
    assert!(services.calls().is_empty());
    owner.dispose().await.unwrap();
    artifacts.close().await;
}

#[tokio::test]
async fn real_retained_read_receives_captured_security_and_budget_identity() {
    let fixture = Fixture::new("", true, 0);
    let Fixture {
        catalog,
        templates,
        services,
        artifacts,
        owner,
        ..
    } = fixture;
    let adapter = CatalogAdapter::new(catalog, planner(), templates.clone()).unwrap();
    let snapshot = ToolSnapshot::document(adapter.materialize().unwrap()).unwrap();
    let selection = selected(89);
    let call = ToolInvocation::new("real-read", r#"{"path":"file.txt"}"#, 9_007_199_254_740_993);
    let ToolHandle::Ordinary(read) = snapshot.find("read").unwrap() else {
        panic!("read must be ordinary")
    };
    assert_eq!(read.parallel_safety(&call), ParallelSafety::ParallelSafe);
    let result = read
        .invoke(
            call,
            InvocationContext::new(
                SharedSecurity(selection.security.clone()),
                selection.clone(),
                &NoCancel,
            ),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "1: alpha\n2: beta\ntotal lines in file: 2\n");
    assert_eq!(
        services
            .calls()
            .iter()
            .find(|call| call.operation == "read")
            .unwrap()
            .security_id,
        Some(71)
    );
    let ToolHandle::Ordinary(image) = snapshot.find("read_image").unwrap() else {
        panic!("image must be ordinary")
    };
    let budget = Arc::new(ToolCycleImageBudget::new(0));
    let mut call = ToolInvocation::new("image", r#"{"path":"pixel.png"}"#, 9);
    call.image_budget = Some(budget.clone());
    call.prompt_templates = Some(templates);
    let output = image
        .invoke(
            call,
            InvocationContext::new(
                SharedSecurity(selection.security.clone()),
                selection,
                &NoCancel,
            ),
        )
        .await
        .unwrap();
    assert_eq!(output.outcome, ToolOutcome::ImageBudgetExceeded);
    assert!(budget.is_exceeded());
    assert!(output.image_artifacts.is_empty());
    owner.dispose().await.unwrap();
    artifacts.close().await;
}

#[tokio::test]
async fn retained_session_uses_immutable_turn_selection_and_boundary_sequence() {
    let fixture = Fixture::new("", true, 0);
    let Fixture {
        catalog,
        templates,
        services,
        artifacts,
        owner,
        ..
    } = fixture;
    let adapter = CatalogAdapter::new(catalog, planner(), templates).unwrap();
    let snapshot = ToolSnapshot::document(adapter.materialize().unwrap()).unwrap();
    let selection = selected(89);
    let invoke = |name: &str, arguments: &str| {
        let ToolHandle::Ordinary(tool) = snapshot.find(name).unwrap() else {
            panic!("ordinary session tool")
        };
        tool.invoke(
            ToolInvocation::new("captured", arguments, 9_007_199_254_740_993),
            InvocationContext::new(
                SharedSecurity(selection.security.clone()),
                selection.clone(),
                &NoCancel,
            ),
        )
    };
    assert!(
        invoke(
            "imagegen",
            r#"{"prompt":"A bird","output_path":"generated.png"}"#
        )
        .await
        .unwrap()
        .text
        .contains("generated.png")
    );
    let calls = services.calls();
    assert_eq!(
        calls
            .iter()
            .find(|call| call.operation == "image_generate")
            .unwrap()
            .selection_id,
        Some(89)
    );
    invoke("set_checkpoint", r#"{"title":"boundary"}"#)
        .await
        .unwrap();
    assert_eq!(
        services.state.lock().unwrap().checkpoints,
        vec![("boundary".into(), 9_007_199_254_740_993, "captured".into())]
    );
    owner.dispose().await.unwrap();
    artifacts.close().await;
}

#[test]
fn output_conversion_preserves_artifacts_opaque_ids_optional_paths_and_activity() {
    use parrot_tools::process::ownership::inventory::{ActivityKind, YieldedShellProcess};
    let artifact = parrot_storage::artifacts::ImageArtifactMetadata {
        artifact_id: "artifact".into(),
        sha256: "sha".into(),
        media_type: "image/png".into(),
        byte_length: 9_007_199_254_740_993,
        width: 1,
        height: 2,
        frame_count: 3,
        aggregate_pixels: 6,
        display_name: "name".into(),
        origin: "origin".into(),
    };
    for kind in [ActivityKind::Execution, ActivityKind::Monitor] {
        let output = output_to_runtime(catalog::Output {
            text: "body".into(),
            image_artifacts: vec![artifact.clone()],
            outcome: files::FileToolOutcome::ImageBudgetExceeded,
            yielded_process: Some(YieldedShellProcess {
                process_id: "00001-opaque".into(),
                name: "work".into(),
                inventory_instance_id: "inventory".into(),
                visible_revision: u64::MAX,
                stdout_path: Some("".into()),
                stderr_path: None,
                activity_kind: kind,
            }),
        });
        assert_eq!(output.text, "body");
        assert_eq!(output.image_artifacts, vec![artifact.clone()]);
        assert_eq!(output.outcome, ToolOutcome::ImageBudgetExceeded);
        let yielded = output.yielded_process.unwrap();
        assert_eq!(yielded.process_id, "00001-opaque");
        assert_eq!(yielded.name, "work");
        assert_eq!(yielded.inventory_instance_id, "inventory");
        assert_eq!(yielded.visible_revision, u64::MAX);
        assert_eq!(yielded.stdout_path, Some("".into()));
        assert_eq!(yielded.stderr_path, None);
        assert_eq!(
            yielded.activity_kind.value(),
            if kind == ActivityKind::Execution {
                0
            } else {
                1
            }
        );
    }
}

#[tokio::test]
async fn invocation_conversion_preserves_templates_call_boundary_and_shared_image_budget() {
    let fixture = Fixture::new("", true, 0);
    let mut call = ToolInvocation::new("call", "raw arguments", i64::MAX);
    let budget = Arc::new(ToolCycleImageBudget::new(10));
    call.image_budget = Some(budget.clone());
    call.prompt_templates = Some(fixture.templates.clone());
    let converted = invocation_to_catalog(call, &fixture.templates);
    assert_eq!(converted.call_id, "call");
    assert_eq!(converted.arguments_json, "raw arguments");
    assert_eq!(converted.assistant_sequence, i64::MAX);
    assert!(Arc::ptr_eq(
        converted.prompt_templates.as_ref().unwrap(),
        &fixture.templates
    ));
    assert!(Arc::ptr_eq(
        &converted.image_budget.as_ref().unwrap().budget,
        &budget
    ));
    assert!(budget.try_accept(11).is_ok_and(|accepted| !accepted));
    assert!(converted.image_budget.unwrap().budget.is_exceeded());
    fixture.owner.dispose().await.unwrap();
    drop(fixture.catalog);
    fixture.artifacts.close().await;
}

#[test]
fn typed_errors_preserve_messages_and_diagnostic_failure_family() {
    use parrot_tools::{
        session::{Failure, FailureKind as SessionFailureKind},
        tasks_queues::{FailureKind as QueueFailureKind, ToolFailure as QueueFailure},
    };
    let cases = vec![
        (
            catalog::Error::Process(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            )),
            FailureKind::AccessDenied,
        ),
        (
            catalog::Error::File(files::FileToolError::GlobTimeout),
            FailureKind::Timeout,
        ),
        (
            catalog::Error::Session(Failure::new(SessionFailureKind::InvalidOperation, "closed")),
            FailureKind::InvalidOperation,
        ),
        (
            catalog::Error::TaskQueue(QueueFailure::new(QueueFailureKind::Argument, "invalid")),
            FailureKind::Argument,
        ),
        (
            catalog::Error::WebFetch(parrot_tools::web_fetch::FetchError::Io(
                std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"),
            )),
            FailureKind::Timeout,
        ),
    ];
    for (error, kind) in cases {
        let message = error.to_string();
        let converted = error_to_runtime(error);
        assert_eq!(converted.message, message);
        assert_eq!(converted.kind, kind);
    }
}

struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
#[tokio::test]
async fn retained_operation_cancellation_is_not_rendered_as_success() {
    let Fixture {
        catalog,
        templates,
        services,
        artifacts,
        owner,
        ..
    } = Fixture::new("", true, 0);
    let adapter = CatalogAdapter::new(catalog, planner(), templates).unwrap();
    let snapshot = ToolSnapshot::document(adapter.materialize().unwrap()).unwrap();
    let ToolHandle::Ordinary(read) = snapshot.find("read").unwrap() else {
        panic!("read must be ordinary")
    };
    let selection = selected(89);
    let result = read
        .invoke(
            ToolInvocation::new("cancel", r#"{"path":"file.txt"}"#, 9),
            InvocationContext::new(
                SharedSecurity(selection.security.clone()),
                selection,
                &Cancelled,
            ),
        )
        .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert!(services.calls().is_empty());
    owner.dispose().await.unwrap();
    artifacts.close().await;
}
