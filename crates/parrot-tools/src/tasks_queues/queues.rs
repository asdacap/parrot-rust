use std::{marker::PhantomData, path::PathBuf, sync::Arc, time::Duration};

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Clock, OperationError, OperationResult},
        filesystem::{
            AuthorizedReadService, EntryKind, FileReadError, FileReadErrorKind, LinkPolicy,
            ReadRequest,
        },
        runtime::AgentQueues,
        security::InvocationContext,
        tools::{ParallelSafety, Tool},
    },
    values::tools::{ConfiguredToolDefinition, MissingToolDefinition, ToolDefinitionCatalog},
};

use super::{FailureKind, InputObject, ToolFailure, ToolInvocation, ToolOutput};

pub const MAXIMUM_SOURCE_FILE_BYTES: usize = 16 << 20;
const MAXIMUM_YIELD_MILLISECONDS: i64 = i64::MAX / 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueToolKind {
    Create,
    Info,
    Push,
    Take,
}

impl QueueToolKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Create => "queue_create",
            Self::Info => "queue_info",
            Self::Push => "queue_push",
            Self::Take => "queue_take",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Unspecified,
    Front,
    Back,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueInfo {
    pub path: String,
    pub name: String,
    pub description: String,
    pub size: i32,
    pub closed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateRequest {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedQueue<Target> {
    pub owner: Target,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueTarget<Target> {
    /// Bare names retain the invoking queue service's own/direct-parent lookup.
    Invoking(String),
    /// An explicit address pins the resolver's actual owner and uses Local only.
    Local(ResolvedQueue<Target>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InfoRequest<Target> {
    pub target: QueueTarget<Target>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushRequest<Target> {
    pub target: QueueTarget<Target>,
    /// JSON null entries reach the store's validation boundary, after lookup.
    pub items: Vec<Option<String>>,
    pub direction: Direction,
    pub close: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeRequest<Target> {
    pub target: QueueTarget<Target>,
    pub count: i32,
    pub direction: Direction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeResult {
    pub acquired: bool,
    /// A contended store can report no acquisition and no current metadata.
    pub info: Option<QueueInfo>,
    pub items: Vec<String>,
}

/// The scoped queue port plus the source resource resolver. Resolved targets
/// retain the actual owner for the invocation; explicit lookup never falls back.
pub trait QueueToolBackend<Target>:
    AgentQueues<
        CreateRequest = CreateRequest,
        CreateResult = QueueInfo,
        InfoRequest = InfoRequest<Target>,
        InfoResult = QueueInfo,
        PushRequest = PushRequest<Target>,
        PushResult = QueueInfo,
        TakeRequest = TakeRequest<Target>,
        TakeResult = TakeResult,
        Error = ToolFailure,
    >
{
    fn resolve(&self, address: &str) -> Result<ResolvedQueue<Target>, ToolFailure>;
    fn session_id(&self) -> &str;
    fn resolved_session_id<'a>(&self, target: &'a Target) -> &'a str;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueDiagnostic {
    pub event: &'static str,
    pub agent_session_id: String,
    pub correlation_id: Option<String>,
    pub duration_milliseconds: Option<u64>,
    pub outcome: Option<&'static str>,
    pub error_code: Option<&'static str>,
}

pub trait QueueDiagnostics: Send + Sync {
    fn record(&self, event: QueueDiagnostic);
}

pub struct QueueTool<Backend, Reader, Time, Diagnostics, Target, Selection> {
    kind: QueueToolKind,
    definition: ConfiguredToolDefinition,
    backend: Arc<Backend>,
    reader: Arc<Reader>,
    clock: Arc<Time>,
    diagnostics: Arc<Diagnostics>,
    types: PhantomData<fn(Target, Selection)>,
}

impl<Q, R, C, D, T, S> QueueTool<Q, R, C, D, T, S>
where
    Q: QueueToolBackend<T>,
    R: AuthorizedReadService,
    C: Clock,
    D: QueueDiagnostics,
    T: Clone + Send + Sync + 'static,
    S: Send + Sync + 'static,
{
    pub fn new(
        kind: QueueToolKind,
        definitions: &ToolDefinitionCatalog,
        backend: Arc<Q>,
        reader: Arc<R>,
        clock: Arc<C>,
        diagnostics: Arc<D>,
    ) -> Result<Self, MissingToolDefinition> {
        Ok(Self {
            kind,
            definition: definitions.describe(kind.name())?.clone(),
            backend,
            reader,
            clock,
            diagnostics,
            types: PhantomData,
        })
    }

    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    pub async fn execute(
        &self,
        invocation: ToolInvocation,
        context: InvocationContext<'_, R::Security, S>,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let result = match self.kind {
            QueueToolKind::Create => {
                self.create(&invocation.arguments_json, context.cancellation())
                    .await
            }
            QueueToolKind::Info => {
                self.info(&invocation.arguments_json, context.cancellation())
                    .await
            }
            QueueToolKind::Push => self.push(&invocation.arguments_json, &context).await,
            QueueToolKind::Take => self.take(&invocation, context.cancellation()).await,
        };
        match result {
            Err(OperationError::Failed(error)) if error.queue_caught() => {
                Ok(ToolOutput::error(error))
            }
            result => result,
        }
    }

    fn target(&self, name: String) -> Result<QueueTarget<T>, ToolFailure> {
        if name.contains('/') {
            self.backend.resolve(&name).map(QueueTarget::Local)
        } else {
            Ok(QueueTarget::Invoking(name))
        }
    }

    async fn create(
        &self,
        arguments: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let input = InputObject::parse(arguments, &["name", "description"])
            .map_err(OperationError::Failed)?;
        let name = input.string("name").map_err(OperationError::Failed)?;
        let description = input
            .string("description")
            .map_err(OperationError::Failed)?
            .unwrap_or_default();
        let info = self
            .backend
            .create(
                CreateRequest {
                    name: require_name(name)?,
                    description,
                },
                cancellation,
            )
            .await?;
        Ok(ToolOutput::text(write_info(&info, None)))
    }

    async fn info(
        &self,
        arguments: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let input = InputObject::parse(arguments, &["name"]).map_err(OperationError::Failed)?;
        let name = require_name(input.string("name").map_err(OperationError::Failed)?)?;
        let target = self.target(name).map_err(OperationError::Failed)?;
        let info = self
            .backend
            .info(InfoRequest { target }, cancellation)
            .await?;
        Ok(ToolOutput::text(write_info(&info, None)))
    }

    async fn push(
        &self,
        arguments: &str,
        context: &InvocationContext<'_, R::Security, S>,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let input = InputObject::parse(
            arguments,
            &["name", "items", "source_file", "direction", "close"],
        )
        .map_err(OperationError::Failed)?;
        let name = input.string("name").map_err(OperationError::Failed)?;
        let items = input.strings("items").map_err(OperationError::Failed)?;
        let source_file = input
            .string("source_file")
            .map_err(OperationError::Failed)?;
        let direction = input.string("direction").map_err(OperationError::Failed)?;
        let close = input
            .boolean("close")
            .map_err(OperationError::Failed)?
            .unwrap_or(false);
        if items.is_some() == source_file.is_some() {
            return Err(format_error(
                "Tool arguments require exactly one of 'items' or 'source_file'.",
            ));
        }
        let items = match items {
            Some(items) => items,
            None => self
                .source_items(source_file.as_deref().unwrap_or_default(), context)
                .await?
                .into_iter()
                .map(Some)
                .collect(),
        };
        let target = self
            .target(require_name(name)?)
            .map_err(OperationError::Failed)?;
        if matches!(target, QueueTarget::Local(_)) {
            context
                .cancellation()
                .check()
                .map_err(|_| OperationError::Cancelled)?;
        }
        let direction = parse_direction(direction.as_deref())?;
        let owner = match &target {
            QueueTarget::Local(resource) => {
                Some(self.backend.resolved_session_id(&resource.owner).to_owned())
            }
            QueueTarget::Invoking(_) => None,
        };
        let info = self
            .backend
            .push(
                PushRequest {
                    target,
                    items,
                    direction,
                    close,
                },
                context.cancellation(),
            )
            .await?;
        if close {
            if let Some(agent_session_id) = owner {
                self.diagnostics.record(QueueDiagnostic {
                    event: "closed",
                    agent_session_id,
                    correlation_id: None,
                    duration_milliseconds: None,
                    outcome: Some("closed"),
                    error_code: None,
                });
            }
        }
        Ok(ToolOutput::text(write_info(&info, None)))
    }

    async fn source_items(
        &self,
        path: &str,
        context: &InvocationContext<'_, R::Security, S>,
    ) -> OperationResult<Vec<String>, ToolFailure> {
        if path.is_empty() {
            return Err(format_error(
                "Tool argument 'source_file' must be a non-empty string.",
            ));
        }
        let path = PathBuf::from(path);
        let metadata = self
            .reader
            .inspect(
                &path,
                LinkPolicy::Follow,
                context.security(),
                context.cancellation(),
            )
            .await
            .map_err(map_source_read)?;
        if metadata.kind == EntryKind::Directory {
            return Err(format_error("source_file must be a file"));
        }
        let request = ReadRequest {
            requested: path,
            expected_physical: Some(metadata.physical_path),
            links: LinkPolicy::Follow,
            max_bytes: Some(MAXIMUM_SOURCE_FILE_BYTES),
        };
        let read = self
            .reader
            .read(&request, context.security(), context.cancellation())
            .await
            .map_err(map_source_read)?;
        decode_source_items(&read.bytes).map_err(OperationError::Failed)
    }

    async fn take(
        &self,
        invocation: &ToolInvocation,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let started = self.clock.now();
        self.diagnostics.record(QueueDiagnostic {
            event: "wait_started",
            agent_session_id: self.backend.session_id().to_owned(),
            correlation_id: Some(invocation.call_id.clone()),
            duration_milliseconds: None,
            outcome: None,
            error_code: None,
        });
        let mut outcome = "failed";
        let result = self
            .take_inner(&invocation.arguments_json, cancellation, &mut outcome)
            .await;
        let error_code = match &result {
            Err(OperationError::Cancelled) => {
                outcome = "cancelled";
                Some("cancelled")
            }
            Err(OperationError::Failed(error)) => Some(classify(error.kind)),
            Ok(_) => None,
        };
        let duration = self
            .clock
            .now()
            .saturating_duration_since(started)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.diagnostics.record(QueueDiagnostic {
            event: "wait_completed",
            agent_session_id: self.backend.session_id().to_owned(),
            correlation_id: Some(invocation.call_id.clone()),
            duration_milliseconds: Some(duration),
            outcome: Some(outcome),
            error_code,
        });
        result
    }

    async fn take_inner(
        &self,
        arguments: &str,
        cancellation: &dyn Cancellation,
        outcome: &mut &'static str,
    ) -> OperationResult<ToolOutput, ToolFailure> {
        let input =
            InputObject::parse(arguments, &["name", "count", "direction", "yield_after_ms"])
                .map_err(OperationError::Failed)?;
        let name = input.string("name").map_err(OperationError::Failed)?;
        let count = input
            .integer32("count")
            .map_err(OperationError::Failed)?
            .unwrap_or(1);
        let direction = input.string("direction").map_err(OperationError::Failed)?;
        let milliseconds = input
            .integer("yield_after_ms")
            .map_err(OperationError::Failed)?
            .unwrap_or(30_000);
        // Resolve an explicit scope before validating polling arguments, as source does.
        let target = self
            .target(require_name(name)?)
            .map_err(OperationError::Failed)?;
        let direction = parse_direction(direction.as_deref())?;
        if count <= 0 {
            return Err(format_error(
                "Tool argument 'count' must be a positive integer.",
            ));
        }
        if !(0..=MAXIMUM_YIELD_MILLISECONDS).contains(&milliseconds) {
            return Err(format_error(
                "Tool argument 'yield_after_ms' must be a non-negative integer within range.",
            ));
        }
        let deadline = self
            .clock
            .now()
            .checked_add(Duration::from_millis(milliseconds as u64))
            .ok_or_else(|| {
                format_error(
                    "Tool argument 'yield_after_ms' must be a non-negative integer within range.",
                )
            })?;
        let mut empty = None;
        loop {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let taken = self
                .backend
                .take(
                    TakeRequest {
                        target: target.clone(),
                        count,
                        direction,
                    },
                    cancellation,
                )
                .await?;
            if taken.acquired {
                *outcome = if taken.items.is_empty() {
                    "closed"
                } else {
                    "delivered"
                };
                let info = taken.info.ok_or_else(|| {
                    OperationError::Failed(ToolFailure::new(
                        FailureKind::Queue,
                        "queue: missing take information",
                    ))
                })?;
                return Ok(ToolOutput::text(write_info(&info, Some(&taken.items))));
            }
            if taken.info.is_some() {
                empty = taken.info;
            }
            let now = self.clock.now();
            if now >= deadline {
                *outcome = "timeout";
                let info = match empty {
                    Some(info) => info,
                    None => {
                        self.backend
                            .info(
                                InfoRequest {
                                    target: target.clone(),
                                },
                                cancellation,
                            )
                            .await?
                    }
                };
                return Ok(ToolOutput::text(write_info(&info, Some(&[]))));
            }
            let next = now
                + deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(10));
            self.clock
                .sleep_until(next, cancellation)
                .await
                .map_err(|_| OperationError::Cancelled)?;
        }
    }
}

impl<Q, R, C, D, T, S> Tool for QueueTool<Q, R, C, D, T, S>
where
    Q: QueueToolBackend<T>,
    R: AuthorizedReadService,
    R::Security: 'static,
    C: Clock,
    D: QueueDiagnostics,
    T: Clone + Send + Sync + 'static,
    S: Send + Sync + 'static,
{
    type Definition = ConfiguredToolDefinition;
    type Invocation = ToolInvocation;
    type Selection = S;
    type Security = R::Security;
    type Output = ToolOutput;
    type Error = ToolFailure;
    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Self::Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        false
    }
    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, OperationResult<Self::Output, Self::Error>> {
        Box::pin(self.execute(invocation, context))
    }
}

fn require_name(name: Option<String>) -> OperationResult<String, ToolFailure> {
    name.ok_or_else(|| format_error("Tool arguments require a string 'name'."))
}

fn parse_direction(direction: Option<&str>) -> OperationResult<Direction, ToolFailure> {
    match direction {
        None => Ok(Direction::Unspecified),
        Some("front") => Ok(Direction::Front),
        Some("back") => Ok(Direction::Back),
        Some(_) => Err(format_error(
            "Tool argument 'direction' must be 'front' or 'back'.",
        )),
    }
}

fn format_error(message: impl Into<String>) -> OperationError<ToolFailure> {
    OperationError::Failed(ToolFailure::format(message))
}

fn map_source_read(error: OperationError<FileReadError>) -> OperationError<ToolFailure> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => {
            let text = match error.kind {
                FileReadErrorKind::NotFound => "no such file or directory".to_owned(),
                FileReadErrorKind::Denied => "access denied".to_owned(),
                FileReadErrorKind::TooLarge => {
                    format!("source_file exceeds {MAXIMUM_SOURCE_FILE_BYTES} bytes")
                }
                FileReadErrorKind::InvalidOperation => error.message,
                _ if error.is_io() => "could not read source_file".to_owned(),
                _ => {
                    return OperationError::Failed(ToolFailure::new(
                        FailureKind::Other,
                        error.message,
                    ));
                }
            };
            format_error(text)
        }
    }
}

pub fn decode_source_items(bytes: &[u8]) -> Result<Vec<String>, ToolFailure> {
    if bytes.len() > MAXIMUM_SOURCE_FILE_BYTES {
        return Err(ToolFailure::format(format!(
            "source_file exceeds {MAXIMUM_SOURCE_FILE_BYTES} bytes"
        )));
    }
    if bytes.contains(&0) {
        return Err(ToolFailure::format("binary file"));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ToolFailure::format("source_file is not valid UTF-8"))?;
    Ok(text
        .split(['\r', '\n'])
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn write_info(info: &QueueInfo, items: Option<&[String]>) -> String {
    let quoted = default_json_quote;
    let mut text = format!(
        "{{\"path\":{},\"name\":{}",
        quoted(&info.path),
        quoted(&info.name)
    );
    if !info.description.is_empty() {
        text.push_str(&format!(",\"description\":{}", quoted(&info.description)));
    }
    text.push_str(&format!(",\"size\":{}", info.size));
    if info.closed || items.is_some() {
        text.push_str(&format!(",\"closed\":{}", info.closed));
    }
    if let Some(items) = items {
        text.push_str(",\"items\":[");
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            text.push_str(&quoted(item));
        }
        text.push(']');
    }
    text.push('}');
    text
}

fn default_json_quote(value: &str) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\u{0008}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{000c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            ' '..='~' if !matches!(character, '"' | '&' | '\'' | '+' | '<' | '>' | '`') => {
                output.push(character)
            }
            character => {
                let mut units = [0; 2];
                for unit in character.encode_utf16(&mut units) {
                    write!(&mut output, "\\u{unit:04X}").expect("writing to text cannot fail");
                }
            }
        }
    }
    output.push('"');
    output
}

fn classify(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Argument => "argument",
        FailureKind::Json
        | FailureKind::Format
        | FailureKind::Registry
        | FailureKind::Queue
        | FailureKind::Other => "unexpected",
        FailureKind::InvalidOperation => "invalid_operation",
        FailureKind::Io => "io",
        FailureKind::Denied => "access_denied",
        FailureKind::Provider => "provider",
    }
}
