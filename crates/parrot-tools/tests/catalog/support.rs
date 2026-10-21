//! Deterministic catalog-binding fixtures, not filesystem policy or host adapters.

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
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, EntryKind, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
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
            CommandRequest, ProcessResolver, ProcessToolServices, ProcessWorkspace, ResolvedProcess,
        },
    },
    session::{
        Compaction, Failure, ImageProvider, PermissionReply, PermissionServices, SessionServices,
        WriteTarget, WriteTargetKind,
    },
    tasks_queues::{
        FailureKind, ToolFailure,
        queues::{
            CreateRequest, Direction, InfoRequest, PushRequest, QueueDiagnostic, QueueDiagnostics,
            QueueInfo, QueueTarget, QueueToolBackend, ResolvedQueue, TakeRequest, TakeResult,
        },
        tasks::{TaskDetail, TaskQuery, TaskSnapshot, TaskUpdate},
    },
};

pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0, 5, 0, 1, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0, 0,
    0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub id: i32,
}
impl AccessSnapshot for Snapshot {
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    pub id: i32,
}

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
                Some(snapshot.id),
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
                Some(snapshot.id),
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
            self.record("read", format!("{request:?}"), Some(snapshot.id), None);
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
                Some(snapshot.id),
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
                Some(snapshot.id),
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
                Some(snapshot.id),
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
            self.record("spawn", format!("{request:?}"), Some(security.id), None);
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
                Some(selection.id),
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
                Some(selection.id),
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
            self.record("compact", format!("{target:?}"), None, Some(selection.id));
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
            Some(selection.id),
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
                Some(selection.id),
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
            Some(snapshot.id),
            None,
        );
        false
    }
    fn is_read_only(&self, snapshot: &Snapshot) -> bool {
        self.record("permission_read_only", "false", Some(snapshot.id), None);
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
                Some(update.selection.id),
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
            let queue = state
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
                Some(security.id),
                None,
            );
            self.services
                .state
                .lock()
                .unwrap()
                .launches
                .push((request, security.id));
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
            .join("../../.tmp/t61a-catalog-artifacts")
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
        let repository = Arc::new(ImageArtifactRepository::new(database.clone(), store).unwrap());
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

#[cfg(test)]
mod fixture_checks {
    use super::*;
    #[tokio::test]
    async fn records_captured_file_and_selection_inputs_without_physical_policy() {
        let services = Services::default();
        let snapshot = Snapshot { id: 71 };
        let read = ReadRequest {
            requested: "file.txt".into(),
            expected_physical: None,
            links: LinkPolicy::Follow,
            max_bytes: None,
        };
        assert_eq!(
            services
                .read(&read, &snapshot, &NoCancel)
                .await
                .unwrap()
                .bytes,
            b"alpha\nbeta"
        );
        assert_eq!(services.calls()[0].security_id, Some(71));
        assert_eq!(
            services
                .runtime_status(&Selection { id: 19 }, &NoCancel)
                .await
                .unwrap(),
            "catalog runtime"
        );
        assert_eq!(services.calls()[1].selection_id, Some(19));
        assert!(snapshot.check_write(Path::new("file.txt")).is_err());
    }
}
