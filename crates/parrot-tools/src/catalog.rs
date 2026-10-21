//! Agent-session-local built-ins. Only family DTOs are adapted here; services,
//! security capture and production lifetime routing remain supplied by the host.

use std::{collections::HashSet, fmt, marker::PhantomData, sync::Arc};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Clock, OperationError},
        filesystem::{AuthorizedMutationService, AuthorizedReadService},
        interactions::{ChildQuestionAnswers, QuestionRequester},
        runtime::{AgentRuntime, TaskController},
        security::{AccessSnapshot, InvocationContext},
        tools::{ParallelSafety, Tool},
    },
    values::tools::{ConfiguredToolDefinition, MissingToolDefinition, ToolDefinitionCatalog},
};
use parrot_foundation::{config::Configuration, template_catalog::PromptTemplateCatalog};
use parrot_storage::artifacts::{ImageArtifactMetadata, ImageArtifactRepository};

use crate::{
    agents::{
        self, AgentSelection, AgentTool, AgentToolError, AgentToolKind, ModelSelector,
        ResultTemplates,
        questions::{AnswerTool, QuestionTool},
    },
    files::{
        self, EditTool, FileToolError, FileToolOutcome, GlobTool, ReadImageTool, ReadTool,
        WorkspacePaths, WriteTool,
    },
    process::{
        ownership::{domain::CapturedProcessLauncher, inventory::YieldedShellProcess},
        tools::{CommandRequest, ProcessToolFactory, ProcessToolKind, ProcessToolServices},
    },
    session::{
        self, ImageGenerationTool, ImageProvider, PermissionServices, RequestWritePermissionTool,
        SessionServices, SessionTool,
    },
    tasks_queues::{
        self, ToolFailure,
        queues::{QueueDiagnostics, QueueTool, QueueToolBackend, QueueToolKind},
        tasks::{TaskQuery, TaskSnapshot, TaskTool, TaskToolKind, TaskUpdate},
    },
};

pub const BUILTIN_NAMES: [&str; 30] = [
    "exec_command",
    "write_stdin",
    "interrupt_process",
    "monitor",
    "question",
    "answer",
    "read",
    "read_image",
    "imagegen",
    "glob",
    "write",
    "edit",
    "web_fetch",
    "agent_spawn",
    "set_agent_tasks",
    "get_agent_tasks",
    "set_checkpoint",
    "set_exit_reminder",
    "clear_exit_reminder",
    "agent_send",
    "agent_interrupt",
    "agent_status",
    "wait",
    "status",
    "compact_context",
    "queue_create",
    "queue_info",
    "queue_push",
    "queue_take",
    "request_write_permission",
];

/// Converts the actual Foundation-loaded definitions without rewriting schemas.
pub fn configured_definitions(configuration: &Configuration) -> ToolDefinitionCatalog {
    ToolDefinitionCatalog::new(
        &configuration
            .runtime
            .tools
            .iter()
            .map(|(name, definition)| {
                (
                    name.clone(),
                    ConfiguredToolDefinition::new(
                        &definition.description,
                        &definition.parameters_json,
                    ),
                )
            })
            .collect(),
    )
}

#[derive(Clone)]
pub struct Invocation {
    pub call_id: String,
    pub assistant_sequence: i64,
    pub arguments_json: String,
    pub prompt_templates: Option<Arc<PromptTemplateCatalog>>,
    pub image_budget: Option<Arc<files::ImageBudget>>,
}

impl Invocation {
    pub fn new(call_id: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            assistant_sequence: 0,
            arguments_json: arguments_json.into(),
            prompt_templates: None,
            image_budget: None,
        }
    }
}

#[derive(Clone)]
pub struct Selection<T> {
    pub requested_model: String,
    pub scoped: T,
}

pub struct Output {
    pub text: String,
    pub image_artifacts: Vec<ImageArtifactMetadata>,
    pub outcome: FileToolOutcome,
    pub yielded_process: Option<YieldedShellProcess>,
}

impl From<String> for Output {
    fn from(text: String) -> Self {
        Self {
            text,
            image_artifacts: Vec::new(),
            outcome: FileToolOutcome::Finished,
            yielded_process: None,
        }
    }
}
impl From<files::FileToolResult> for Output {
    fn from(result: files::FileToolResult) -> Self {
        Self {
            text: result.text,
            image_artifacts: result.image_artifacts,
            outcome: result.outcome,
            yielded_process: None,
        }
    }
}
impl From<crate::process::tools::ProcessToolResult> for Output {
    fn from(result: crate::process::tools::ProcessToolResult) -> Self {
        Self {
            yielded_process: result.yielded_process,
            ..result.text.into()
        }
    }
}
impl From<session::Output> for Output {
    fn from(result: session::Output) -> Self {
        result.text.into()
    }
}
impl From<tasks_queues::ToolOutput> for Output {
    fn from(result: tasks_queues::ToolOutput) -> Self {
        result.text.into()
    }
}

#[derive(Debug)]
pub enum Error {
    File(FileToolError),
    Process(std::io::Error),
    Agent(AgentToolError),
    Session(session::Failure),
    TaskQueue(ToolFailure),
    WebFetch(crate::web_fetch::FetchError),
    Definition(MissingToolDefinition),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(e) => e.fmt(f),
            Self::Process(e) => e.fmt(f),
            Self::Agent(e) => e.fmt(f),
            Self::Session(e) => e.fmt(f),
            Self::TaskQueue(e) => e.fmt(f),
            Self::WebFetch(e) => e.fmt(f),
            Self::Definition(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
macro_rules! family_error {
    ($kind:ident, $error:ty) => {
        impl From<$error> for Error {
            fn from(error: $error) -> Self {
                Self::$kind(error)
            }
        }
    };
}
family_error!(File, FileToolError);
family_error!(Process, std::io::Error);
family_error!(Agent, AgentToolError);
family_error!(Session, session::Failure);
family_error!(TaskQueue, ToolFailure);
family_error!(WebFetch, crate::web_fetch::FetchError);
family_error!(Definition, MissingToolDefinition);

impl From<Invocation> for crate::web_fetch::Invocation {
    fn from(i: Invocation) -> Self {
        Self {
            arguments_json: i.arguments_json,
            prompt_templates: i.prompt_templates,
        }
    }
}
impl From<crate::web_fetch::Output> for Output {
    fn from(result: crate::web_fetch::Output) -> Self {
        result.text.into()
    }
}

impl From<Invocation> for files::FileToolInvocation {
    fn from(i: Invocation) -> Self {
        Self {
            call_id: i.call_id,
            arguments_json: i.arguments_json,
            prompt_templates: i.prompt_templates,
            image_budget: i.image_budget,
        }
    }
}
impl From<Invocation> for crate::process::tools::ProcessInvocation {
    fn from(i: Invocation) -> Self {
        Self {
            call_id: i.call_id,
            arguments_json: i.arguments_json,
            prompt_templates: i.prompt_templates,
        }
    }
}
impl From<Invocation> for agents::ToolInvocation {
    fn from(i: Invocation) -> Self {
        Self {
            call_id: i.call_id,
            arguments_json: i.arguments_json,
            assistant_sequence: i.assistant_sequence,
        }
    }
}
impl From<Invocation> for tasks_queues::ToolInvocation {
    fn from(i: Invocation) -> Self {
        Self {
            call_id: i.call_id,
            arguments_json: i.arguments_json,
            assistant_sequence: i.assistant_sequence,
        }
    }
}
impl From<Invocation> for session::Invocation {
    fn from(i: Invocation) -> Self {
        Self {
            call_id: i.call_id,
            assistant_sequence: i.assistant_sequence,
            arguments_json: i.arguments_json,
            prompt_templates: i.prompt_templates,
        }
    }
}

pub type RetainedTool<S, T> = dyn Tool<
        Definition = ConfiguredToolDefinition,
        Invocation = Invocation,
        Selection = Selection<T>,
        Security = S,
        Output = Output,
        Error = Error,
    >;

/// A factory slot retains the one tool created for this agent session. Cloning
/// its handle never creates a new tool or resets invocation-local observers.
pub struct Factory<S, T> {
    name: &'static str,
    aliases: &'static [&'static str],
    supported: bool,
    tool: Arc<RetainedTool<S, T>>,
}
impl<S, T> Clone for Factory<S, T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            aliases: self.aliases,
            supported: self.supported,
            tool: Arc::clone(&self.tool),
        }
    }
}
impl<S: AccessSnapshot + 'static, T: Send + 'static> Factory<S, T> {
    pub fn name(&self) -> &'static str {
        self.name
    }
    pub fn aliases(&self) -> &'static [&'static str] {
        self.aliases
    }
    pub fn definition(&self) -> &ConfiguredToolDefinition {
        self.tool.definition()
    }
    pub fn supports(&self) -> bool {
        self.supported
    }
    pub fn create(&self) -> Arc<RetainedTool<S, T>> {
        Arc::clone(&self.tool)
    }
}

struct Binding<U: Tool, T> {
    tool: U,
    selection: fn(&Selection<T>) -> U::Selection,
    marker: PhantomData<fn() -> T>,
}
impl<U, T> Tool for Binding<U, T>
where
    U: Tool<Definition = ConfiguredToolDefinition>,
    U::Security: Clone,
    U::Invocation: From<Invocation>,
    U::Output: Into<Output>,
    U::Error: Into<Error>,
    T: Send + Sync + 'static,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = Selection<T>;
    type Security = U::Security;
    type Output = Output;
    type Error = Error;
    fn definition(&self) -> &Self::Definition {
        self.tool.definition()
    }
    fn parallel_safety(&self, invocation: &Invocation) -> ParallelSafety {
        self.tool.parallel_safety(&invocation.clone().into())
    }
    fn enabled_after_interruption(&self) -> bool {
        self.tool.enabled_after_interruption()
    }
    fn invoke<'a>(
        &'a self,
        invocation: Invocation,
        context: InvocationContext<'a, Self::Security, Selection<T>>,
    ) -> BoxFuture<'a, Result<Output, OperationError<Error>>> {
        let selection = (self.selection)(context.selection());
        let context = InvocationContext::new(
            context.security().clone(),
            selection,
            context.cancellation(),
        );
        Box::pin(async move {
            self.tool
                .invoke(invocation.into(), context)
                .await
                .map(Into::into)
                .map_err(|e| match e {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(e) => OperationError::Failed(e.into()),
                })
        })
    }
}
fn bind<U, T>(
    name: &'static str,
    aliases: &'static [&'static str],
    supported: bool,
    tool: U,
    selection: fn(&Selection<T>) -> U::Selection,
) -> Factory<U::Security, T>
where
    U: Tool<Definition = ConfiguredToolDefinition> + 'static,
    U::Security: Clone,
    U::Invocation: From<Invocation>,
    U::Output: Into<Output>,
    U::Error: Into<Error>,
    T: Send + Sync + 'static,
{
    Factory {
        name,
        aliases,
        supported,
        tool: Arc::new(Binding {
            tool,
            selection,
            marker: PhantomData,
        }),
    }
}
fn unit<T>(_: &Selection<T>) {}
fn scoped<T: Clone>(selection: &Selection<T>) -> T {
    selection.scoped.clone()
}
fn agent<T>(selection: &Selection<T>) -> AgentSelection {
    AgentSelection {
        requested_model: selection.requested_model.clone(),
    }
}

pub struct FileFactories<S, T> {
    read: Factory<S, T>,
    image: Factory<S, T>,
    glob: Factory<S, T>,
    write: Factory<S, T>,
    edit: Factory<S, T>,
}
impl<S: AccessSnapshot + Clone + 'static, T: Send + Sync + 'static> FileFactories<S, T> {
    pub fn new<F>(
        filesystem: F,
        paths: WorkspacePaths,
        artifacts: Arc<ImageArtifactRepository>,
        maximum_image_bytes: i32,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, MissingToolDefinition>
    where
        F: AuthorizedReadService<Security = S>
            + AuthorizedMutationService<Security = S>
            + Clone
            + 'static,
    {
        Ok(Self {
            read: bind(
                "read",
                &[],
                true,
                ReadTool::new(
                    filesystem.clone(),
                    paths.clone(),
                    definitions.describe("read")?.clone(),
                ),
                unit,
            ),
            image: bind(
                "read_image",
                &[],
                true,
                ReadImageTool::new(
                    filesystem.clone(),
                    artifacts,
                    maximum_image_bytes,
                    definitions.describe("read_image")?.clone(),
                ),
                unit,
            ),
            glob: bind(
                "glob",
                &[],
                true,
                GlobTool::new(filesystem.clone(), definitions.describe("glob")?.clone()),
                unit,
            ),
            write: bind(
                "write",
                &[],
                true,
                WriteTool::new(
                    filesystem.clone(),
                    paths.clone(),
                    definitions.describe("write")?.clone(),
                ),
                unit,
            ),
            edit: bind(
                "edit",
                &[],
                true,
                EditTool::new(filesystem, paths, definitions.describe("edit")?.clone()),
                unit,
            ),
        })
    }
}

pub struct ProcessFactories<S, T>([Factory<S, T>; 4]);
impl<S: AccessSnapshot + Clone + 'static, T: Send + Sync + 'static> ProcessFactories<S, T> {
    pub fn new<L>(
        services: Arc<ProcessToolServices<L>>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, MissingToolDefinition>
    where
        L: CapturedProcessLauncher<Request = CommandRequest, Security = S> + 'static,
    {
        let create = |kind: ProcessToolKind| {
            Ok(bind(
                kind.name(),
                kind.aliases(),
                true,
                ProcessToolFactory::new(kind, services.clone(), definitions)?.create(),
                unit,
            ))
        };
        Ok(Self([
            create(ProcessToolKind::ExecCommand)?,
            create(ProcessToolKind::WriteStdin)?,
            create(ProcessToolKind::InterruptProcess)?,
            create(ProcessToolKind::Monitor)?,
        ]))
    }
}

pub struct AgentFactories<S, T> {
    question: Factory<S, T>,
    answer: Factory<S, T>,
    spawn: Factory<S, T>,
    send: Factory<S, T>,
    interrupt: Factory<S, T>,
    status: Factory<S, T>,
    wait: Factory<S, T>,
}
pub struct AgentServices<R, M, Q, A> {
    pub runtime: Arc<R>,
    pub models: Arc<M>,
    pub questions: Arc<Q>,
    pub answers: Arc<A>,
    pub templates: Arc<dyn ResultTemplates>,
    pub allow_parent: bool,
}
impl<S: AccessSnapshot + Clone + 'static, T: Send + Sync + 'static> AgentFactories<S, T> {
    pub fn new<R, M, Q, A>(
        services: AgentServices<R, M, Q, A>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, Error>
    where
        R: AgentRuntime<
                Security = S,
                SpawnRequest = agents::SpawnRequest,
                SpawnResult = agents::SpawnResult,
                SendRequest = agents::SendRequest,
                SendResult = agents::SendResult,
                InterruptRequest = agents::NamedRequest,
                InterruptResult = (),
                StatusRequest = agents::NamedRequest,
                StatusResult = agents::status::StatusSnapshot,
                WaitRequest = agents::input::WaitInput,
                WaitResult = String,
                Error = AgentToolError,
            > + 'static,
        M: ModelSelector + 'static,
        Q: QuestionRequester<Error = AgentToolError> + 'static,
        A: ChildQuestionAnswers<Error = AgentToolError> + 'static,
    {
        let definition = |name| {
            definitions
                .describe(name)
                .cloned()
                .map_err(Error::Definition)
        };
        let create = |kind: AgentToolKind| -> Result<Factory<S, T>, Error> {
            let mut configured = definition(kind.name())?;
            if kind == AgentToolKind::Send {
                configured = agents::send_definition(
                    configured,
                    services.allow_parent,
                    services.templates.as_ref(),
                )?;
            }
            Ok(bind(
                kind.name(),
                &[],
                true,
                AgentTool::new(
                    kind,
                    configured,
                    services.runtime.clone(),
                    services.models.clone(),
                    Some(services.templates.clone()),
                    services.allow_parent,
                ),
                agent,
            ))
        };
        Ok(Self {
            question: bind(
                "question",
                &[],
                true,
                QuestionTool::<_, S>::new(
                    definition("question")?,
                    services.questions,
                    Some(services.templates.clone()),
                ),
                agent,
            ),
            answer: bind(
                "answer",
                &[],
                true,
                AnswerTool::<_, S>::new(
                    definition("answer")?,
                    services.answers,
                    Some(services.templates.clone()),
                ),
                agent,
            ),
            spawn: create(AgentToolKind::Spawn)?,
            send: create(AgentToolKind::Send)?,
            interrupt: create(AgentToolKind::Interrupt)?,
            status: create(AgentToolKind::Status)?,
            wait: create(AgentToolKind::Wait)?,
        })
    }
}

pub struct SessionFactories<S, T> {
    image: Factory<S, T>,
    checkpoint: Factory<S, T>,
    set_reminder: Factory<S, T>,
    clear_reminder: Factory<S, T>,
    status: Factory<S, T>,
    compact: Factory<S, T>,
    permission: Factory<S, T>,
}
pub struct SessionBindings<C, I, P, F> {
    pub control: Arc<C>,
    pub images: Arc<I>,
    pub permission: Arc<P>,
    pub filesystem: Arc<F>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub agent_depth: i32,
}
impl<S: AccessSnapshot + Clone + 'static, T: Clone + Send + Sync + 'static> SessionFactories<S, T> {
    pub fn new<C, I, P, F>(
        services: SessionBindings<C, I, P, F>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, MissingToolDefinition>
    where
        C: SessionServices<Security = S, Selection = T> + 'static,
        I: ImageProvider<Security = S, Selection = T> + 'static,
        P: PermissionServices<Security = S, Selection = T> + 'static,
        F: AuthorizedReadService<Security = S> + AuthorizedMutationService<Security = S> + 'static,
    {
        let create = |kind: session::Kind| {
            Ok(bind(
                kind.name(),
                &[],
                true,
                SessionTool::new(
                    kind,
                    definitions.describe(kind.name())?.clone(),
                    services.control.clone(),
                    services.templates.clone(),
                ),
                scoped,
            ))
        };
        Ok(Self {
            image: bind(
                "imagegen",
                &[],
                true,
                ImageGenerationTool::new(
                    definitions.describe("imagegen")?.clone(),
                    services.images,
                    services.filesystem,
                ),
                scoped,
            ),
            checkpoint: create(session::Kind::SetCheckpoint)?,
            set_reminder: create(session::Kind::SetExitReminder)?,
            clear_reminder: create(session::Kind::ClearExitReminder)?,
            status: create(session::Kind::Status)?,
            compact: create(session::Kind::CompactContext)?,
            permission: bind(
                "request_write_permission",
                &[],
                services.agent_depth == 0,
                RequestWritePermissionTool::new(
                    definitions.describe("request_write_permission")?.clone(),
                    services.permission,
                ),
                scoped,
            ),
        })
    }
}

pub struct TaskQueueFactories<S, T> {
    set: Factory<S, T>,
    get: Factory<S, T>,
    queues: [Factory<S, T>; 4],
}
pub struct TaskQueueServices<C, F, B, K, D> {
    pub tasks: Arc<C>,
    pub filesystem: Arc<F>,
    pub templates: Arc<PromptTemplateCatalog>,
    pub queues: Arc<B>,
    pub clock: Arc<K>,
    pub diagnostics: Arc<D>,
}
impl<S: AccessSnapshot + Clone + 'static, T: Clone + Send + Sync + 'static>
    TaskQueueFactories<S, T>
{
    pub fn new<C, F, B, K, D, Target>(
        services: TaskQueueServices<C, F, B, K, D>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, MissingToolDefinition>
    where
        C: TaskController<
                Update = TaskUpdate<T>,
                Query = TaskQuery,
                Snapshot = TaskSnapshot,
                Error = ToolFailure,
            > + 'static,
        F: AuthorizedReadService<Security = S> + 'static,
        B: QueueToolBackend<Target> + 'static,
        K: Clock + 'static,
        D: QueueDiagnostics + 'static,
        Target: Clone + Send + Sync + 'static,
    {
        let render_accepted = Arc::new(move |count: usize| {
            services
                .templates
                .render(
                    "agent-task.set-accepted",
                    &[("count".into(), serde_json::Value::String(count.to_string()))],
                    &crate::process::ownership::execution::NeverCancelled,
                )
                .map_err(|error| {
                    ToolFailure::new(tasks_queues::FailureKind::Other, error.to_string())
                })
        });
        let task = |kind| {
            Ok(bind(
                TaskToolKind::name(kind),
                &[],
                true,
                TaskTool::<_, _, _, T>::new(
                    kind,
                    definitions,
                    services.tasks.clone(),
                    services.filesystem.clone(),
                    render_accepted.clone(),
                )?,
                scoped,
            ))
        };
        let queue = |kind| {
            Ok(bind(
                QueueToolKind::name(kind),
                &[],
                true,
                QueueTool::<_, _, _, _, Target, T>::new(
                    kind,
                    definitions,
                    services.queues.clone(),
                    services.filesystem.clone(),
                    services.clock.clone(),
                    services.diagnostics.clone(),
                )?,
                scoped,
            ))
        };
        Ok(Self {
            set: task(TaskToolKind::Set)?,
            get: task(TaskToolKind::Get)?,
            queues: [
                queue(QueueToolKind::Create)?,
                queue(QueueToolKind::Info)?,
                queue(QueueToolKind::Push)?,
                queue(QueueToolKind::Take)?,
            ],
        })
    }
}

pub struct WebFetchFactory<S, T>(Factory<S, T>);
impl<S: AccessSnapshot + Clone + 'static, T: Clone + Send + Sync + 'static> WebFetchFactory<S, T> {
    pub fn new(
        fetcher: Arc<crate::web_fetch::WebFetcher>,
        definitions: &ToolDefinitionCatalog,
    ) -> Result<Self, MissingToolDefinition> {
        Ok(Self(bind(
            "web_fetch",
            &[],
            true,
            crate::web_fetch::WebFetchTool::<S, T>::new(
                fetcher,
                definitions.describe("web_fetch")?.clone(),
            ),
            scoped,
        )))
    }
}

pub struct Catalog<S, T> {
    factories: Vec<Factory<S, T>>,
}
impl<S: AccessSnapshot + 'static, T: Send + 'static> Catalog<S, T> {
    pub fn new(
        process: ProcessFactories<S, T>,
        agents: AgentFactories<S, T>,
        files: FileFactories<S, T>,
        session: SessionFactories<S, T>,
        tasks: TaskQueueFactories<S, T>,
        web: WebFetchFactory<S, T>,
    ) -> Self {
        let [exec, stdin, interrupt_process, monitor] = process.0;
        let [queue_create, queue_info, queue_push, queue_take] = tasks.queues;
        Self {
            factories: vec![
                exec,
                stdin,
                interrupt_process,
                monitor,
                agents.question,
                agents.answer,
                files.read,
                files.image,
                session.image,
                files.glob,
                files.write,
                files.edit,
                web.0,
                agents.spawn,
                tasks.set,
                tasks.get,
                session.checkpoint,
                session.set_reminder,
                session.clear_reminder,
                agents.send,
                agents.interrupt,
                agents.status,
                agents.wait,
                session.status,
                session.compact,
                queue_create,
                queue_info,
                queue_push,
                queue_take,
                session.permission,
            ],
        }
    }
    pub fn factories(&self) -> &[Factory<S, T>] {
        &self.factories
    }
    pub fn snapshot(&self) -> Result<Snapshot<S, T>, SnapshotError> {
        Snapshot::document(
            &self.factories,
            &self
                .factories
                .iter()
                .map(Factory::supports)
                .collect::<Vec<_>>(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotError(pub String);
impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SnapshotError {}

pub struct Snapshot<S, T> {
    entries: Vec<Factory<S, T>>,
}
impl<S, T> Clone for Snapshot<S, T> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}
impl<S: AccessSnapshot + 'static, T: Send + 'static> Snapshot<S, T> {
    pub fn document(
        factories: &[Factory<S, T>],
        supported: &[bool],
    ) -> Result<Self, SnapshotError> {
        if factories.len() != supported.len() {
            return Err(SnapshotError(
                "Tool support flags and definitions must match the tool inventory.".into(),
            ));
        }
        let mut names = HashSet::new();
        let mut entries = Vec::new();
        for (factory, supported) in factories.iter().zip(supported) {
            for name in std::iter::once(factory.name()).chain(factory.aliases().iter().copied()) {
                if !names.insert(name) {
                    return Err(SnapshotError(format!(
                        "tool '{name}' is registered more than once"
                    )));
                }
            }
            if *supported {
                entries.push(factory.clone());
            }
        }
        Ok(Self { entries })
    }
    pub fn factories(&self) -> &[Factory<S, T>] {
        &self.entries
    }
    pub fn definitions(&self) -> Vec<parrot_provider::sessions::model::Tool> {
        self.entries
            .iter()
            .map(|factory| parrot_provider::sessions::model::Tool {
                name: factory.name().into(),
                description: factory.definition().description.clone(),
                parameters_json: factory.definition().parameters_json.clone(),
            })
            .collect()
    }
    pub fn find(&self, name: &str) -> Option<&Factory<S, T>> {
        self.entries
            .iter()
            .find(|f| f.name() == name || f.aliases().contains(&name))
    }
    pub fn permitted_by(&self, is_permitted: impl Fn(&str) -> bool) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|f| is_permitted(f.name()))
                .cloned()
                .collect(),
        }
    }
    pub fn enabled_after_interruption(&self) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|f| f.tool.enabled_after_interruption())
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "../tests/catalog/snapshot_cases.rs"]
mod snapshot_cases;
