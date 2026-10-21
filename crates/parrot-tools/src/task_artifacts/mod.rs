//! Parsing and validated file loading for AgentTask artifacts. The injected
//! filesystem service owns authorization; this module never accesses files itself.
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::Path,
};

use parrot_domain::contracts::{
    agent_tasks::TaskArtifactParser,
    async_ops::{Cancellation, OperationError, OperationResult},
    filesystem::{AuthorizedReadService, FileReadError, LinkPolicy, ReadRequest},
};

pub use parrot_domain::values::agent_tasks::{
    ARTIFACT_VERSION, AcceptanceVerdict, ExecutionStatus, LeafResponse, MAX_DESCRIPTION_LENGTH,
    Task, TaskArtifact, TaskPayload,
};

/// Tools-owned adapter for runtime callers of the shared parser contract. It
/// forwards to this module's single parser implementation and carries no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskArtifactParserAdapter;

impl TaskArtifactParser for TaskArtifactParserAdapter {
    type Error = ParseError;

    fn parse_artifact(&self, input: &str) -> Result<TaskArtifact, Self::Error> {
        parse_artifact(input)
    }

    fn parse_task_set(&self, input: &str) -> Result<Vec<Task>, Self::Error> {
        parse_task_set(input)
    }

    fn validate_merged_graph(&self, tasks: &[Task], path: &str) -> Result<(), Self::Error> {
        validate_merged_graph(tasks, path)
    }

    fn parse_leaf_response(&self, input: &str) -> Result<LeafResponse, Self::Error> {
        parse_leaf_response(input)
    }
}

const MAX_JSON_DEPTH: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError(String);
impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ParseError {}

// Ordered raw children retain both duplicate properties and numeric lexemes.
// This matches the source's later typed-field validation and preserves valid
// JSON number tokens (for example 1e400) until their field type is checked.
struct OrderedObject(Vec<(String, Box<RawValue>)>);
impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = OrderedObject;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry::<String, Box<RawValue>>()? {
                    fields.push(field);
                }
                Ok(OrderedObject(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

pub fn parse_artifact(input: &str) -> Result<TaskArtifact, ParseError> {
    let json = extract_envelope(input)?;
    let root = parse_json(&json)?;
    let fields = object(&root, "artifact")?;
    reject_unknown(&fields, "artifact", &["schema_version", "tasks"])?;
    let schema_version = number(
        property(&fields, "schema_version")
            .ok_or_else(|| ParseError::new("schema_version is required."))?,
        "schema_version",
    )?;
    if schema_version != ARTIFACT_VERSION {
        return Err(ParseError::new("schema_version must equal 1."));
    }
    let tasks = parse_tasks(
        &array(
            property(&fields, "tasks").ok_or_else(|| ParseError::new("tasks is required."))?,
            "tasks",
        )?,
        "tasks",
    )?;
    validate_graph(&tasks, "tasks")?;
    Ok(TaskArtifact {
        schema_version,
        tasks,
    })
}

/// Source-compatible file-loading error. The service's typed filesystem error
/// remains available to the caller so it can apply the original tool catch
/// boundaries without collapsing authorization, parsing, and cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactReadError {
    Read(FileReadError),
    Parse(ParseError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeforeToolBatch {
    pub assistant_sequence: i64,
    pub call_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskInputError {
    Arguments(&'static str),
    Artifact(ArtifactReadError),
    Inline(ParseError),
}

impl fmt::Display for TaskInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(message) => formatter.write_str(message),
            Self::Artifact(error) => error.fmt(formatter),
            Self::Inline(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TaskInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Arguments(_) => None,
            Self::Artifact(error) => Some(error),
            Self::Inline(error) => Some(error),
        }
    }
}

impl fmt::Display for ArtifactReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(formatter),
            Self::Parse(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ArtifactReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Parse(error) => Some(error),
        }
    }
}

/// Select and parse the already-deserialized `set_agent_tasks` input. Presence
/// is kept separate from value conversion so a present null/non-string path
/// receives the source nonblank-path error rather than being mistaken for an
/// absent path. No task controller is called here; callers can dispatch only
/// after this function succeeds.
pub async fn parse_task_input<S>(
    path_present: bool,
    path: Option<&str>,
    tasks_present: bool,
    tasks_json: &str,
    service: &S,
    snapshot: &S::Security,
    cancellation: &dyn Cancellation,
) -> OperationResult<Vec<Task>, TaskInputError>
where
    S: AuthorizedReadService,
{
    if path_present == tasks_present {
        return Err(OperationError::Failed(TaskInputError::Arguments(
            "Tool arguments require exactly one of a nonblank string 'path' or a 'tasks' array.",
        )));
    }
    if path_present {
        let Some(path) = path.filter(|path| !path.trim().is_empty()) else {
            return Err(OperationError::Failed(TaskInputError::Arguments(
                "Tool arguments require a nonblank string 'path'.",
            )));
        };
        return read_artifact(service, Path::new(path), snapshot, cancellation)
            .await
            .map(|artifact| artifact.tasks)
            .map_err(|error| match error {
                OperationError::Cancelled => OperationError::Cancelled,
                OperationError::Failed(error) => {
                    OperationError::Failed(TaskInputError::Artifact(error))
                }
            });
    }

    parse_task_set(tasks_json)
        .map_err(|error| OperationError::Failed(TaskInputError::Inline(error)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskApplicationError<DispatchError> {
    Input(TaskInputError),
    Dispatch(DispatchError),
}

impl<DispatchError: fmt::Display> fmt::Display for TaskApplicationError<DispatchError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => error.fmt(formatter),
            Self::Dispatch(error) => error.fmt(formatter),
        }
    }
}

impl<DispatchError> std::error::Error for TaskApplicationError<DispatchError>
where
    DispatchError: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(error) => Some(error),
            Self::Dispatch(error) => Some(error),
        }
    }
}

pub struct TaskInputInvocation<'a, Security, Selection> {
    pub path_present: bool,
    pub path: Option<&'a str>,
    pub tasks_present: bool,
    pub tasks_json: &'a str,
    pub snapshot: &'a Security,
    pub cancellation: &'a dyn Cancellation,
    pub selection: &'a Selection,
    pub assistant_sequence: i64,
    pub call_id: &'a str,
}

/// Parse one source input and synchronously apply the resulting graph. The
/// selection is borrowed unchanged and the source BeforeToolBatch identity is
/// constructed from this invocation's assistant sequence and call ID. The
/// callback is invoked exactly once only after successful input/read/parse;
/// callers render the returned accepted count only after this function succeeds.
pub async fn apply_task_input<S, Selection, DispatchError, Dispatch>(
    service: &S,
    invocation: TaskInputInvocation<'_, S::Security, Selection>,
    dispatch: Dispatch,
) -> OperationResult<usize, TaskApplicationError<DispatchError>>
where
    S: AuthorizedReadService,
    Dispatch: FnOnce(Vec<Task>, &Selection, BeforeToolBatch) -> Result<(), DispatchError>,
{
    let tasks = parse_task_input(
        invocation.path_present,
        invocation.path,
        invocation.tasks_present,
        invocation.tasks_json,
        service,
        invocation.snapshot,
        invocation.cancellation,
    )
    .await
    .map_err(|error| match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => OperationError::Failed(TaskApplicationError::Input(error)),
    })?;
    let accepted_count = tasks.len();
    let boundary = BeforeToolBatch {
        assistant_sequence: invocation.assistant_sequence,
        call_id: invocation.call_id.to_owned(),
    };
    dispatch(tasks, invocation.selection, boundary)
        .map_err(|error| OperationError::Failed(TaskApplicationError::Dispatch(error)))?;
    Ok(accepted_count)
}

/// Read one artifact through the injected authorization boundary, then decode
/// exactly like `new StreamReader(stream)` (BOM detection and replacement
/// decoding) before applying the in-memory parser. The source artifact call has
/// no byte cap, so this uses `max_bytes: None`; parsing never grants dispatch or
/// filesystem authority.
pub async fn read_artifact<S>(
    service: &S,
    requested: &Path,
    snapshot: &S::Security,
    cancellation: &dyn Cancellation,
) -> OperationResult<TaskArtifact, ArtifactReadError>
where
    S: AuthorizedReadService,
{
    let request = ReadRequest {
        requested: requested.to_path_buf(),
        expected_physical: None,
        links: LinkPolicy::RejectLinks,
        max_bytes: None,
    };
    let read = service
        .read(&request, snapshot, cancellation)
        .await
        .map_err(|error| match error {
            OperationError::Cancelled => OperationError::Cancelled,
            OperationError::Failed(error) => OperationError::Failed(ArtifactReadError::Read(error)),
        })?;
    let text = decode_stream_reader_bytes(&read.bytes);
    parse_artifact(&text).map_err(|error| OperationError::Failed(ArtifactReadError::Parse(error)))
}

pub(crate) fn decode_stream_reader_bytes(bytes: &[u8]) -> String {
    const UTF32_LE: &[u8] = &[0xFF, 0xFE, 0x00, 0x00];
    const UTF32_BE: &[u8] = &[0x00, 0x00, 0xFE, 0xFF];
    const UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];
    const UTF16_LE: &[u8] = &[0xFF, 0xFE];
    const UTF16_BE: &[u8] = &[0xFE, 0xFF];

    if let Some(body) = bytes.strip_prefix(UTF32_LE) {
        return decode_utf32(body, true);
    }
    if let Some(body) = bytes.strip_prefix(UTF32_BE) {
        return decode_utf32(body, false);
    }
    if let Some(body) = bytes.strip_prefix(UTF8) {
        return String::from_utf8_lossy(body).into_owned();
    }
    if let Some(body) = bytes.strip_prefix(UTF16_LE) {
        return decode_utf16(body, true);
    }
    if let Some(body) = bytes.strip_prefix(UTF16_BE) {
        return decode_utf16(body, false);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let mut units = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();
    let mut text = char::decode_utf16(units.drain(..))
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect::<String>();
    if bytes.len() % 2 != 0 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn decode_utf32(bytes: &[u8], little_endian: bool) -> String {
    let mut text = bytes
        .chunks_exact(4)
        .map(|chunk| {
            let unit = if little_endian {
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            } else {
                u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            };
            char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect::<String>();
    if bytes.len() % 4 != 0 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

pub fn parse_task_set(input: &str) -> Result<Vec<Task>, ParseError> {
    let root = parse_json(input)?;
    parse_tasks(&array(&root, "tasks")?, "tasks")
}

pub fn validate_merged_graph(tasks: &[Task], path: &str) -> Result<(), ParseError> {
    validate_graph(tasks, path)
}

pub fn parse_leaf_response(input: &str) -> Result<LeafResponse, ParseError> {
    let json = extract_envelope(input)?;
    let root = parse_json(&json)?;
    let fields = object(&root, "leaf response")?;
    let result = required_string(&fields, "result", "leaf response")?;
    let verdict = required_string(&fields, "verdict", "leaf response")?;
    let verdict = match verdict.as_str() {
        "accept" => {
            reject_unknown(&fields, "leaf response", &["result", "verdict"])?;
            AcceptanceVerdict::Accept
        }
        "reject_and_halt" => {
            reject_unknown(&fields, "leaf response", &["result", "verdict", "feedback"])?;
            AcceptanceVerdict::RejectAndHalt {
                feedback: required_string(&fields, "feedback", "leaf response")?,
            }
        }
        "reject_and_retry" => {
            reject_unknown(
                &fields,
                "leaf response",
                &[
                    "result",
                    "verdict",
                    "feedback",
                    "payload",
                    "replacement_result",
                ],
            )?;
            AcceptanceVerdict::RejectAndRetry {
                feedback: required_string(&fields, "feedback", "leaf response")?,
                payload: optional_string(&fields, "payload", "leaf response")?,
                replacement_result: optional_string(
                    &fields,
                    "replacement_result",
                    "leaf response",
                )?,
            }
        }
        _ => {
            return Err(ParseError::new(
                "leaf response verdict must be accept, reject_and_halt, or reject_and_retry.",
            ));
        }
    };
    Ok(LeafResponse { result, verdict })
}

fn parse_json(s: &str) -> Result<Box<RawValue>, ParseError> {
    let value = serde_json::from_str::<Box<RawValue>>(s)
        .map_err(|e| ParseError::new(format!("Invalid JSON: {e}")))?;
    if !within_source_json_depth(value.get()) {
        return Err(ParseError::new("Invalid JSON: maximum depth exceeded."));
    }
    Ok(value)
}

fn extract_envelope(s: &str) -> Result<String, ParseError> {
    let text = trim_bom(s);
    let mut blocks = Vec::new();
    let mut start = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        if body.trim_start().starts_with("```") {
            if let Some(b) = start.take() {
                blocks.push(&text[b..pos])
            } else {
                start = Some(pos + line.len())
            }
        }
        pos += line.len();
    }
    if let Some(b) = start {
        if b < text.len() {
            blocks.push(&text[b..])
        }
    }
    for b in blocks.into_iter().rev() {
        let candidate = trim_bom(b);
        if is_envelope(candidate) {
            return Ok(candidate.to_owned());
        }
    }
    for (i, c) in text.char_indices() {
        if c == '{' || c == '[' {
            let candidate = &text[i..];
            if is_envelope(candidate) {
                return Ok(candidate.to_owned());
            }
        }
    }
    Err(ParseError::new(
        "The response does not end with a JSON object or array envelope.",
    ))
}

fn trim_bom(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix('\u{feff}').unwrap_or(value).trim()
}

fn is_envelope(s: &str) -> bool {
    let Ok(value) = parse_json(s) else {
        return false;
    };
    matches!(value.get().as_bytes().first(), Some(b'{') | Some(b'['))
}

#[cfg(test)]
#[path = "stream_reader_tests.rs"]
mod stream_reader_tests;

fn within_source_json_depth(value: &str) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in value.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_JSON_DEPTH {
                        return false;
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    true
}

fn parse_tasks(values: &[Box<RawValue>], path: &str) -> Result<Vec<Task>, ParseError> {
    if values.is_empty() {
        return Err(ParseError::new(format!("{path} must not be empty.")));
    }
    let mut out = Vec::new();
    for (i, v) in values.iter().enumerate() {
        out.push(parse_task(v, &format!("{path}[{i}]"))?)
    }
    let mut names = HashSet::new();
    for t in &out {
        if !names.insert(t.name.as_str()) {
            return Err(ParseError::new(format!(
                "{path} contains duplicate task name '{}'.",
                t.name
            )));
        }
        if t.dependencies.iter().any(|d| d == &t.name) {
            return Err(ParseError::new(format!(
                "{path} task '{}' cannot depend on itself.",
                t.name
            )));
        }
    }
    Ok(out)
}

fn parse_task(v: &RawValue, path: &str) -> Result<Task, ParseError> {
    let fields = object(v, path)?;
    reject_unknown(
        &fields,
        path,
        &[
            "name",
            "dependencies",
            "description",
            "payload",
            "acceptance_criteria",
            "model",
            "hidden",
            "state",
            "result",
            "failure",
        ],
    )?;
    let name = required_string(&fields, "name", path)?;
    let description = required_string(&fields, "description", path)?;
    if description.encode_utf16().count() > MAX_DESCRIPTION_LENGTH {
        return Err(ParseError::new(format!(
            "{path} description must be at most {MAX_DESCRIPTION_LENGTH} characters."
        )));
    }
    let acceptance_criteria = required_string(&fields, "acceptance_criteria", path)?;
    let raw_payload =
        property(&fields, "payload").ok_or_else(|| ParseError::new("payload is required."))?;
    let payload = match first_non_whitespace(raw_payload.get()) {
        Some(b'"') => {
            let instruction = string(raw_payload, &format!("{path} payload"))?;
            nonblank(&instruction, &format!("{path} payload"))?;
            TaskPayload::Instruction(instruction)
        }
        Some(b'[') => {
            let children = array(raw_payload, &format!("{path} payload"))?;
            let tasks = parse_tasks(&children, &format!("{path} payload"))?;
            validate_graph(&tasks, &format!("{path} payload"))?;
            TaskPayload::Tasks(tasks)
        }
        _ => {
            return Err(ParseError::new(format!(
                "{path} payload has an invalid value."
            )));
        }
    };
    let dependencies = dependencies(&fields, path)?;
    let model = optional_string(&fields, "model", path)?;
    let hidden = optional_bool(&fields, "hidden", path)?;
    let state = match optional_string(&fields, "state", path)?.as_deref() {
        None | Some("pending") => ExecutionStatus::Pending,
        Some("running") => ExecutionStatus::Running,
        Some("succeeded") => ExecutionStatus::Succeeded,
        Some("failed") => ExecutionStatus::Failed,
        Some("canceled") => ExecutionStatus::Canceled,
        Some(_) => {
            return Err(ParseError::new(format!(
                "{path} state must be pending, running, succeeded, failed, or canceled."
            )));
        }
    };
    let result = optional_string(&fields, "result", path)?;
    let failure = optional_string(&fields, "failure", path)?;
    Ok(Task {
        name,
        dependencies,
        description,
        payload,
        acceptance_criteria,
        model,
        hidden,
        state,
        result,
        failure,
    })
}

fn dependencies(fields: &OrderedObject, path: &str) -> Result<Vec<String>, ParseError> {
    let Some(value) = property(fields, "dependencies") else {
        return Ok(Vec::new());
    };
    let values = array(value, &format!("{path} dependencies must be an array."))?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in &values {
        let dependency = string(value, &format!("{path} dependencies must contain strings."))
            .map_err(|_| ParseError::new(format!("{path} dependencies must contain strings.")))?;
        nonblank(&dependency, &format!("{path} dependency"))?;
        if !seen.insert(dependency.clone()) {
            return Err(ParseError::new(format!(
                "{path} dependencies must be distinct."
            )));
        }
        out.push(dependency)
    }
    Ok(out)
}

fn optional_string(
    fields: &OrderedObject,
    key: &str,
    path: &str,
) -> Result<Option<String>, ParseError> {
    match property(fields, key) {
        None => Ok(None),
        Some(raw) => {
            let value = string(raw, &format!("{path} {key}"))
                .map_err(|_| ParseError::new(format!("{path} {key} must be a string.")))?;
            nonblank(&value, &format!("{path} {key}"))?;
            Ok(Some(value))
        }
    }
}

fn optional_bool(fields: &OrderedObject, key: &str, path: &str) -> Result<bool, ParseError> {
    match property(fields, key) {
        None => Ok(false),
        Some(value) => serde_json::from_str::<bool>(value.get())
            .map_err(|_| ParseError::new(format!("{path} {key} must be a boolean."))),
    }
}

fn required_string(fields: &OrderedObject, key: &str, path: &str) -> Result<String, ParseError> {
    let value =
        property(fields, key).ok_or_else(|| ParseError::new(format!("{key} is required.")))?;
    let value = string(value, &format!("{path} {key}"))
        .map_err(|_| ParseError::new(format!("{path} {key} must be a string.")))?;
    nonblank(&value, &format!("{path} {key}"))?;
    Ok(value)
}

fn nonblank(value: &str, path: &str) -> Result<(), ParseError> {
    if value.trim().is_empty() {
        Err(ParseError::new(format!("{path} must be nonblank.")))
    } else {
        Ok(())
    }
}

fn first_non_whitespace(value: &str) -> Option<u8> {
    value
        .bytes()
        .find(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
}

fn property<'a>(fields: &'a OrderedObject, key: &str) -> Option<&'a RawValue> {
    fields
        .0
        .iter()
        .rev()
        .find_map(|(name, value)| (name == key).then_some(value.as_ref()))
}

fn object(value: &RawValue, path: &str) -> Result<OrderedObject, ParseError> {
    serde_json::from_str(value.get())
        .map_err(|_| ParseError::new(format!("{path} must be an object.")))
}

fn array(value: &RawValue, error: &str) -> Result<Vec<Box<RawValue>>, ParseError> {
    serde_json::from_str(value.get()).map_err(|_| ParseError::new(error))
}

fn string(value: &RawValue, path: &str) -> Result<String, ParseError> {
    serde_json::from_str(value.get())
        .map_err(|_| ParseError::new(format!("{path} must be a string.")))
}

fn number(value: &RawValue, path: &str) -> Result<u64, ParseError> {
    serde_json::from_str::<i32>(value.get())
        .map(|number| number as u64)
        .map_err(|_| ParseError::new(format!("{path} has an invalid value.")))
}

fn reject_unknown(value: &OrderedObject, path: &str, allowed: &[&str]) -> Result<(), ParseError> {
    let mut seen = HashSet::new();
    for (key, _) in &value.0 {
        if !seen.insert(key.as_str()) {
            return Err(ParseError::new(format!(
                "{path} contains duplicate field '{key}'."
            )));
        }
        if !allowed.contains(&key.as_str()) {
            return Err(ParseError::new(format!(
                "{path} contains unknown field '{key}'."
            )));
        }
    }
    Ok(())
}

fn validate_graph(tasks: &[Task], path: &str) -> Result<(), ParseError> {
    let mut map = HashMap::new();
    for task in tasks {
        if map.insert(task.name.as_str(), task).is_some() {
            return Err(ParseError::new(format!(
                "{path} contains duplicate task name '{}'.",
                task.name
            )));
        }
    }
    for task in tasks {
        for dependency in &task.dependencies {
            if !map.contains_key(dependency.as_str()) {
                return Err(ParseError::new(format!(
                    "{path} task '{}' has missing sibling dependency '{}'.",
                    task.name, dependency
                )));
            }
        }
    }
    let (mut visiting, mut done) = (HashSet::new(), HashSet::new());
    for task in tasks {
        visit(task, &map, &mut visiting, &mut done, path)?
    }
    Ok(())
}

fn visit<'a>(
    task: &'a Task,
    map: &HashMap<&'a str, &'a Task>,
    visiting: &mut HashSet<&'a str>,
    done: &mut HashSet<&'a str>,
    path: &str,
) -> Result<(), ParseError> {
    if done.contains(task.name.as_str()) {
        return Ok(());
    }
    if !visiting.insert(task.name.as_str()) {
        return Err(ParseError::new(format!(
            "{path} contains a dependency cycle."
        )));
    }
    for dependency in &task.dependencies {
        visit(map[dependency.as_str()], map, visiting, done, path)?
    }
    visiting.remove(task.name.as_str());
    done.insert(task.name.as_str());
    Ok(())
}
