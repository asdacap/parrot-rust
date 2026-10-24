//! Source operands: ExecCommandToolTests, MonitorToolTests, and
//! ShellProcessInteractionTests. The pass-through launcher below owns real OS
//! processes but provides no sandbox, filesystem authorization, or T36 approval.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext};
use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_domain::protocol::messages::parrot::{Delivery, InputSource};
use parrot_domain::values::tools::{ConfiguredToolDefinition, ToolDefinitionCatalog};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_tools::process::execution::{LaunchSpec, ShellProcessExecution, TerminalMode};
use parrot_tools::process::ownership::domain::CapturedProcessLauncher;
use parrot_tools::process::ownership::execution::{Execution, NeverCancelled, ProcessLifetime};
use parrot_tools::process::ownership::inventory::ActivityKind;
use parrot_tools::process::ownership::managed::{
    CompletionMessage, CompletionSink, NoProcessDiagnostics,
};
use parrot_tools::process::ownership::monitor::{
    MonitorDelivery, MonitorEvent, MonitorIdentity, MonitorInputSource, MonitorReport, MonitorSink,
};
use parrot_tools::process::ownership::owner::{ProcessAgent, ShellProcessOwner};
use parrot_tools::process::tools::{
    CommandRequest, ProcessInvocation, ProcessResolver, ProcessTool, ProcessToolFactory,
    ProcessToolKind, ProcessToolResult, ProcessToolServices, ProcessWorkspace, ResolvedProcess,
};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(20);
const METADATA_REMINDER: &str = "\n\nReminder: include 'name' and 'description' for multiline commands or commands longer than 300 characters.";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp")
            .join(format!(
                "t57-wrapper-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct CapturedSecurity(Arc<u64>);
impl AccessSnapshot for CapturedSecurity {
    type Error = io::Error;

    fn check_read(&self, _: &ResolvedRead) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture capture denies filesystem access",
        ))
    }

    fn check_write(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture capture denies filesystem access",
        ))
    }
}

struct LaunchRecord {
    request: CommandRequest,
    marker: Arc<u64>,
}

/// Explicit synthetic launch planning, not production sandbox composition.
struct PassThroughLauncher {
    shell: PathBuf,
    workspace: PathBuf,
    blobs: PathBuf,
    environment: BTreeMap<String, String>,
    records: Mutex<Vec<LaunchRecord>>,
    executions: Mutex<Vec<Arc<ShellProcessExecution>>>,
    failure: Mutex<Option<(io::ErrorKind, String)>>,
}
impl CapturedProcessLauncher for PassThroughLauncher {
    type Request = CommandRequest;
    type Security = CapturedSecurity;

    fn launch(
        &self,
        request: CommandRequest,
        security: CapturedSecurity,
        lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async move {
            let mut spec = LaunchSpec::new(&self.shell);
            spec.args = vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                request.command.clone().into(),
            ];
            spec.current_dir = Some(self.workspace.clone());
            spec.environment.extend(
                self.environment
                    .iter()
                    .map(|(key, value)| (key.into(), Some(value.into()))),
            );
            spec.environment.extend(
                request
                    .environment
                    .iter()
                    .map(|(key, value)| (key.into(), Some(value.into()))),
            );
            self.records.lock().unwrap().push(LaunchRecord {
                request: request.clone(),
                marker: security.0,
            });
            if let Some((kind, message)) = self.failure.lock().unwrap().clone() {
                return Err(io::Error::new(kind, message));
            }
            let mut execution = match request.terminal_mode {
                TerminalMode::Pipe => ShellProcessExecution::start_pipe(&spec, &self.blobs)?,
                TerminalMode::PseudoTerminal => {
                    let helper = std::env::var_os("T57_PTY_HELPER")
                        .or_else(|| option_env!("PARROT_PTY_HELPER").map(Into::into))
                        .expect("PTY wrapper tests require the retained built helper; no pipe fallback is permitted");
                    ShellProcessExecution::start_pty(&spec, Path::new(&helper), &self.blobs, 24, 80)
                        .await?
                }
            };
            execution.bind_cancellation(lifetime)?;
            let execution = Arc::new(execution);
            self.executions.lock().unwrap().push(Arc::clone(&execution));
            Ok(execution as Arc<dyn Execution>)
        })
    }
}

struct Workspace(String);
impl ProcessWorkspace for Workspace {
    fn root(&self) -> &str {
        &self.0
    }

    // This metadata query is deliberately not a security check.
    fn resolves_to_root(&self, target: &str) -> bool {
        fs::canonicalize(target).is_ok_and(|actual| actual == Path::new(&self.0))
    }
}

#[derive(Default)]
struct Resolver {
    routes: Mutex<BTreeMap<String, (Arc<ShellProcessOwner>, String)>>,
    calls: Mutex<Vec<String>>,
}
impl Resolver {
    fn route(&self, address: &str, owner: &Arc<ShellProcessOwner>, name: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(address.into(), (Arc::clone(owner), name.into()));
    }
}
impl ProcessResolver for Resolver {
    fn resolve(&self, address: &str) -> io::Result<ResolvedProcess> {
        self.calls.lock().unwrap().push(address.into());
        self.routes
            .lock()
            .unwrap()
            .get(address)
            .map(|(owner, name)| ResolvedProcess {
                owner: Arc::clone(owner),
                name: name.clone(),
            })
            .ok_or_else(|| io::Error::other("resource owner unavailable"))
    }
}

#[derive(Clone, Debug)]
enum Row {
    Event(MonitorEvent),
    Completion(CompletionMessage),
}
struct RecordingSink {
    templates: Arc<PromptTemplateCatalog>,
    rows: Mutex<Vec<Row>>,
    failures: Mutex<Vec<String>>,
}
impl RecordingSink {
    fn render(&self, key: &str, arguments: &[(&str, String)]) -> io::Result<String> {
        let arguments = arguments
            .iter()
            .map(|(key, value)| ((*key).into(), Value::String(value.clone())))
            .collect::<Vec<_>>();
        self.templates
            .render(key, &arguments, &NeverCancelled)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    async fn rows(&self, count: usize) -> Vec<Row> {
        tokio::time::timeout(LIMIT, async {
            loop {
                let rows = self.rows.lock().unwrap().clone();
                if rows.len() >= count {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real owner did not deliver expected rows")
    }
}
impl CompletionSink for RecordingSink {
    fn send<'a>(
        &'a self,
        message: &'a CompletionMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
            self.rows
                .lock()
                .unwrap()
                .push(Row::Completion(message.clone()));
            Ok(())
        })
    }
}
impl MonitorSink for RecordingSink {
    fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String> {
        self.render(
            "monitor.event",
            &[
                ("name", identity.name.clone()),
                ("description", identity.description.clone()),
                ("lines", lines.into()),
            ],
        )
    }

    fn persist_event<'a>(
        &'a self,
        _: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<String>> {
        // All source rows below are inline; do not pretend to persist a large event.
        Box::pin(async {
            Err(io::Error::other(
                "unexpected oversized monitor event in inline-row fixture",
            ))
        })
    }

    fn send_event<'a>(
        &'a self,
        event: MonitorEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
            self.rows.lock().unwrap().push(Row::Event(event));
            Ok(())
        })
    }

    fn observe_failed(&self, _: &MonitorIdentity, failure: &io::Error) {
        self.failures.lock().unwrap().push(failure.to_string());
    }

    fn render_completion(
        &self,
        identity: &MonitorIdentity,
        report: &MonitorReport,
    ) -> io::Result<String> {
        self.render(
            "monitor.ended",
            &[
                ("name", identity.name.clone()),
                ("description", identity.description.clone()),
                ("events", report.events.to_string()),
                ("reason", report.reason.as_str().into()),
                ("exit_code", report.exit_code.clone()),
                ("error", report.error.clone()),
                ("timeout_ms", report.timeout_ms.clone()),
            ],
        )
    }
}

struct Fixture {
    directory: Directory,
    owner: Arc<ShellProcessOwner>,
    launcher: Arc<PassThroughLauncher>,
    resolver: Arc<Resolver>,
    sink: Arc<RecordingSink>,
    definitions: ToolDefinitionCatalog,
    services: Arc<ProcessToolServices<PassThroughLauncher>>,
    security: CapturedSecurity,
}
impl Fixture {
    fn new() -> Self {
        let directory = Directory::new();
        let environment =
            BTreeMap::from([("HOME".into(), directory.0.to_string_lossy().into_owned())]);
        let (configuration, templates) =
            PromptTemplateCatalog::from_content("", &environment).unwrap();
        let definitions = ToolDefinitionCatalog::new(
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
        );
        let templates = Arc::new(templates);
        let sink = Arc::new(RecordingSink {
            templates: Arc::clone(&templates),
            rows: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
        });
        let owner_id = format!(
            "t57-owner-{}",
            directory.0.file_name().unwrap().to_string_lossy()
        );
        let owner = Arc::new(
            ShellProcessOwner::new(
                owner_id.clone(),
                None,
                sink.clone(),
                Arc::new(NoProcessDiagnostics),
            )
            .unwrap(),
        );
        let resolver = Arc::new(Resolver::default());
        let environment = BTreeMap::from([
            ("WORKDIR".into(), directory.0.to_string_lossy().into_owned()),
            (
                "SCRATCH_DIR".into(),
                directory
                    .0
                    .join("shared scratch")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "AGENT_SCRATCH_DIR".into(),
                directory
                    .0
                    .join("agent scratch")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "AGENT_HISTORY_DIR".into(),
                directory.0.join("history").to_string_lossy().into_owned(),
            ),
        ]);
        for path in environment.values() {
            fs::create_dir_all(path).unwrap();
        }
        let blobs = directory.0.join("blobs");
        fs::create_dir(&blobs).unwrap();
        let launcher = Arc::new(PassThroughLauncher {
            shell: std::env::var_os("T57_TEST_SHELL")
                .expect("set T57_TEST_SHELL to the explicit retained Nix bash")
                .into(),
            workspace: directory.0.clone(),
            blobs,
            environment,
            records: Mutex::new(Vec::new()),
            executions: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        });
        let services = Arc::new(ProcessToolServices {
            owner: Arc::clone(&owner),
            agent: ProcessAgent {
                session_id: owner_id,
                name: "wrapper-fixture".into(),
                parent_session_id: "parent-session".into(),
                parent_name: "parent".into(),
                depth: 1,
            },
            launcher: Arc::clone(&launcher),
            resolver: resolver.clone(),
            workspace: Arc::new(Workspace(directory.0.to_string_lossy().into_owned())),
            read_only_prefixes: configuration
                .runtime
                .read_only_exec_command_prefixes
                .as_slice()
                .to_vec(),
            templates,
            monitor_sink: sink.clone(),
        });
        Self {
            directory,
            owner,
            launcher,
            resolver,
            sink,
            definitions,
            services,
            security: CapturedSecurity(Arc::new(73)),
        }
    }

    fn tool(&self, kind: ProcessToolKind) -> ProcessTool<PassThroughLauncher> {
        ProcessToolFactory::new(kind, Arc::clone(&self.services), &self.definitions)
            .unwrap()
            .create()
    }

    async fn invoke(&self, kind: ProcessToolKind, arguments: Value) -> ProcessToolResult {
        self.invoke_json(kind, &arguments.to_string()).await
    }

    async fn invoke_json(&self, kind: ProcessToolKind, arguments: &str) -> ProcessToolResult {
        tokio::time::timeout(
            LIMIT,
            self.tool(kind).invoke(
                ProcessInvocation::new("wrapper-call", arguments),
                InvocationContext::new(self.security.clone(), (), &NeverCancelled),
            ),
        )
        .await
        .expect("wrapper invocation timed out")
        .unwrap()
    }

    async fn exec(&self, arguments: Value) -> ProcessToolResult {
        self.invoke(ProcessToolKind::ExecCommand, arguments).await
    }

    async fn finish(&self) {
        tokio::time::timeout(LIMIT, self.owner.dispose())
            .await
            .expect("owner disposal timed out")
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Also terminate the fixture's OS processes on an assertion/timeout failure.
        for execution in self.launcher.executions.lock().unwrap().iter() {
            execution.request_cancel();
        }
    }
}

fn completed(text: &str, exit: i32, suffix: &str) {
    assert!(
        text.starts_with(&format!("Process exited with code {exit} after ")),
        "{text}"
    );
    assert!(text.ends_with(suffix), "{text}");
}

async fn live_text(path: &Path, expected: &str) {
    tokio::time::timeout(LIMIT, async {
        loop {
            if fs::read_to_string(path).is_ok_and(|actual| actual == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native live output did not arrive");
}

#[tokio::test]
async fn factory_uses_actual_configured_definitions_names_aliases_and_schemas() {
    let fixture = Fixture::new();
    for (kind, name) in [
        (ProcessToolKind::ExecCommand, "exec_command"),
        (ProcessToolKind::WriteStdin, "write_stdin"),
        (ProcessToolKind::InterruptProcess, "interrupt_process"),
        (ProcessToolKind::Monitor, "monitor"),
    ] {
        let factory =
            ProcessToolFactory::new(kind, Arc::clone(&fixture.services), &fixture.definitions)
                .unwrap();
        let tool = factory.create();
        assert_eq!(
            factory.definition(),
            fixture.definitions.describe(name).unwrap()
        );
        assert_eq!(tool.definition(), factory.definition());
        assert_eq!(tool.name(), name);
        assert_eq!(
            tool.aliases(),
            if kind == ProcessToolKind::ExecCommand {
                &["bash"][..]
            } else {
                &[]
            }
        );
        assert_eq!(
            tool.enabled_after_interruption(),
            kind == ProcessToolKind::InterruptProcess
        );
        assert!(!tool.definition().description.is_empty());
        let schema: Value = serde_json::from_str(&tool.definition().parameters_json).unwrap();
        assert_eq!(schema["type"], "object");
        match kind {
            ProcessToolKind::ExecCommand => {
                assert_eq!(schema["required"], json!(["command"]));
                assert_eq!(schema["additionalProperties"], false);
                assert_eq!(
                    schema["properties"]["env"]["additionalProperties"]["type"],
                    "string"
                );
                assert_eq!(schema["properties"]["tty"]["default"], false);
                assert_eq!(schema["properties"]["yield_after_ms"]["minimum"], 500);
                assert_eq!(schema["properties"]["yield_after_ms"]["maximum"], 10000);
            }
            ProcessToolKind::WriteStdin => {
                assert_eq!(schema["required"], json!(["name", "input"]));
                assert_eq!(schema["properties"]["yield_after_ms"]["default"], 250);
                assert_eq!(schema["properties"]["yield_after_ms"]["minimum"], 0);
            }
            ProcessToolKind::InterruptProcess => {
                assert_eq!(schema["required"], json!(["name"]));
                assert_eq!(schema["properties"]["signal"]["default"], 2);
                assert_eq!(schema["properties"]["signal"]["minimum"], 1);
                assert_eq!(schema["properties"]["signal"]["maximum"], 64);
            }
            ProcessToolKind::Monitor => {
                assert_eq!(schema["required"], json!(["command", "description"]));
                assert_eq!(schema["additionalProperties"], false);
                assert_eq!(schema["properties"]["timeout_ms"]["default"], 300000);
                assert_eq!(schema["properties"]["timeout_ms"]["minimum"], 1000);
                assert_eq!(schema["properties"]["timeout_ms"]["maximum"], 1800000);
            }
        }
    }
    fixture.finish().await;
}

#[tokio::test]
async fn real_completion_environment_and_owned_security_capture_reach_the_launcher() {
    let fixture = Fixture::new();
    let result = fixture
        .exec(json!({"command":"printf out; printf err >&2; exit 7"}))
        .await;
    completed(&result.text, 7, "s\n[stdout]\nout\n[stderr]\nerr");
    assert!(result.yielded_process.is_none());
    assert!(fixture.owner.active().await.is_empty());
    assert!(fixture.sink.rows.lock().unwrap().is_empty());

    for (environment, expected) in [
        (json!({"COMMAND_VALUE":"available"}), "<available>"),
        (json!({"COMMAND_VALUE":""}), "<>"),
        (
            json!({"COMMAND_VALUE":"$(printf injected); * ' quoted"}),
            "<$(printf injected); * ' quoted>",
        ),
    ] {
        let result = fixture
            .exec(json!({"command":"printf '<%s>' \"$COMMAND_VALUE\"", "env":environment}))
            .await;
        completed(&result.text, 0, &format!("s\n[stdout]\n{expected}"));
    }
    let inherited = fixture
        .exec(json!({"command":"printf '%s' \"$PATH\""}))
        .await;
    completed(
        &inherited.text,
        0,
        &format!("s\n[stdout]\n{}", std::env::var("PATH").unwrap()),
    );
    let command = "printf '<%s>' \"$WORKDIR\" \"${SCRATCH_DIR}\" \"$AGENT_SCRATCH_DIR\" \"${AGENT_HISTORY_DIR}\"";
    let expected = [
        "WORKDIR",
        "SCRATCH_DIR",
        "AGENT_SCRATCH_DIR",
        "AGENT_HISTORY_DIR",
    ]
    .map(|key| format!("<{}>", fixture.launcher.environment[key]))
    .concat();
    let original = fixture.exec(json!({"command":command})).await;
    completed(&original.text, 0, &format!("s\n[stdout]\n{expected}"));
    let empty = fixture.exec(json!({"command":command,"env":{"WORKDIR":"","SCRATCH_DIR":"","AGENT_SCRATCH_DIR":"","AGENT_HISTORY_DIR":""}})).await;
    completed(&empty.text, 0, "s\n[stdout]\n<><><><>");
    let literal = fixture.exec(json!({"command":command,"env":{"WORKDIR":"$SCRATCH_DIR","SCRATCH_DIR":"${WORKDIR}/literal path","AGENT_SCRATCH_DIR":"$(printf injected); * ' quoted","AGENT_HISTORY_DIR":"`printf injected`"}})).await;
    completed(
        &literal.text,
        0,
        "s\n[stdout]\n<$SCRATCH_DIR><${WORKDIR}/literal path><$(printf injected); * ' quoted><`printf injected`>",
    );
    let reset = fixture.exec(json!({"command":command})).await;
    completed(&reset.text, 0, &format!("s\n[stdout]\n{expected}"));
    {
        let records = fixture.launcher.records.lock().unwrap();
        assert!(
            records
                .iter()
                .all(|record| Arc::ptr_eq(&record.marker, &fixture.security.0))
        );
        assert_eq!(*records[0].marker, 73);
        assert_eq!(
            records[0].request.command,
            "printf out; printf err >&2; exit 7"
        );
        assert_eq!(records[0].request.terminal_mode, TerminalMode::Pipe);
        assert_eq!(
            records[1].request.environment,
            vec![("COMMAND_VALUE".into(), "available".into())]
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn invalid_wrapper_arguments_have_source_exact_errors_and_never_launch() {
    let fixture = Fixture::new();
    for (kind, arguments, expected) in [
        (
            ProcessToolKind::ExecCommand,
            "{}",
            "error: Tool arguments require a string 'command'.",
        ),
        (
            ProcessToolKind::ExecCommand,
            "[]",
            "error: Tool arguments require a string 'command'.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":""}"#,
            "error: no command given",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","name":" "}"#,
            "error: process name must not be empty",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","yield_after_ms":10001}"#,
            "error: Tool argument 'yield_after_ms' must be a non-negative integer no greater than 10000.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","env":{"VALUE":1}}"#,
            "error: Tool argument 'env' must contain only string values.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","env":[]}"#,
            "error: Tool argument 'env' must be an object containing string values.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","env":{"":"value"}}"#,
            "error: Tool argument 'env' contains an invalid environment value.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","env":{"INVALID=NAME":"value"}}"#,
            "error: Tool argument 'env' contains an invalid environment value.",
        ),
        (
            ProcessToolKind::ExecCommand,
            r#"{"command":"true","env":{"VALUE":"\u0000"}}"#,
            "error: Tool argument 'env' contains an invalid environment value.",
        ),
        (
            ProcessToolKind::WriteStdin,
            "{}",
            "error: Tool arguments require a string 'name'.",
        ),
        (
            ProcessToolKind::WriteStdin,
            r#"{"name":"process"}"#,
            "error: Tool arguments require a string 'input'.",
        ),
        (
            ProcessToolKind::WriteStdin,
            r#"{"name":" ","input":""}"#,
            "error: process name must not be empty",
        ),
        (
            ProcessToolKind::InterruptProcess,
            "[]",
            "error: Tool arguments require a string 'name'.",
        ),
        (
            ProcessToolKind::InterruptProcess,
            r#"{"name":" "}"#,
            "error: process name must not be empty",
        ),
        (
            ProcessToolKind::InterruptProcess,
            r#"{"name":"x","signal":"9"}"#,
            "error: Tool argument 'signal' must be an integer.",
        ),
        (
            ProcessToolKind::InterruptProcess,
            r#"{"name":"x","signal":0}"#,
            "error: Tool argument 'signal' must be between 1 and 64.",
        ),
        (
            ProcessToolKind::InterruptProcess,
            r#"{"name":"x","signal":65}"#,
            "error: Tool argument 'signal' must be between 1 and 64.",
        ),
        (
            ProcessToolKind::Monitor,
            r#"{"command":"true"}"#,
            "error: description must not be empty",
        ),
        (
            ProcessToolKind::Monitor,
            r#"{"command":"","description":"d"}"#,
            "error: no command given",
        ),
        (
            ProcessToolKind::Monitor,
            r#"{"command":"true","description":"d","name":" "}"#,
            "error: process name must not be empty",
        ),
        (
            ProcessToolKind::Monitor,
            r#"{"command":"true","description":"d","timeout_ms":999}"#,
            "error: Tool argument 'timeout_ms' must be between 1000 and 1800000.",
        ),
        (
            ProcessToolKind::Monitor,
            r#"{"command":"true","description":"d","timeout_ms":1800001}"#,
            "error: Tool argument 'timeout_ms' must be between 1000 and 1800000.",
        ),
    ] {
        let result = fixture.invoke_json(kind, arguments).await;
        assert_eq!(result.text, expected, "{arguments}");
        assert!(result.yielded_process.is_none());
    }
    assert!(fixture.launcher.records.lock().unwrap().is_empty());
    assert!(fixture.owner.snapshot().await.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn redundant_cd_and_leading_sleep_rejections_do_not_reserve_names() {
    let fixture = Fixture::new();
    let root = fixture.directory.0.to_str().unwrap();
    for (command, target) in [
        (format!("cd {root} && pwd"), root),
        ("cd . && pwd".into(), "."),
        ("cd $WORKDIR && pwd".into(), "$WORKDIR"),
        ("cd ${WORKDIR} && pwd".into(), "${WORKDIR}"),
        (format!("cd \"{root}\" && pwd"), root),
        (format!("cd {root}"), root),
    ] {
        let result = fixture.exec(json!({"command":command})).await;
        assert_eq!(
            result.text,
            format!(
                "error: current directory is already '{target}'; do not change directory to conserve token."
            )
        );
    }
    for command in [
        "sleep 30",
        "sleep",
        "   \tsleep 1",
        "sleep;echo done",
        "sleep 1 && echo done",
        "sleep 1\nprintf done",
    ] {
        let result = fixture
            .exec(json!({"command":command,"name":"not-reserved","yield_after_ms":0}))
            .await;
        assert_eq!(
            result.text,
            "error: leading 'sleep' is not allowed in exec_command; use the wait tool to pause instead."
        );
        assert!(result.yielded_process.is_none());
    }
    assert!(fixture.launcher.records.lock().unwrap().is_empty());
    let reused = fixture
        .exec(json!({"command":"printf free","name":"not-reserved"}))
        .await;
    completed(&reused.text, 0, "s\n[stdout]\nfree");
    let sub = fixture.directory.0.join("sub");
    fs::create_dir(&sub).unwrap();
    let changed = fixture.exec(json!({"command":"cd sub && pwd"})).await;
    completed(
        &changed.text,
        0,
        &format!("s\n[stdout]\n{}\n", sub.display()),
    );
    let trailing = fixture
        .exec(json!({"command":format!("printf x && cd {root}")}))
        .await;
    completed(&trailing.text, 0, "s\n[stdout]\nx");
    let embedded = fixture
        .exec(json!({"command":"printf x; sleep 0.05; printf y"}))
        .await;
    completed(&embedded.text, 0, "s\n[stdout]\nxy");
    let qualified = fixture.exec(json!({"command":"command sleep 0"})).await;
    assert!(
        qualified
            .text
            .starts_with("Process exited with code 0 after ")
    );
    assert!(!qualified.text.contains("[stdout]") && !qualified.text.contains("[stderr]"));
    let sleepy = fixture.exec(json!({"command":"sleepy 2"})).await;
    assert!(sleepy.text.starts_with("Process exited with code "));
    assert!(!sleepy.text.contains("leading 'sleep' is not allowed"));
    fixture.finish().await;
}

#[tokio::test]
async fn metadata_reminder_uses_real_default_template_for_completion_and_yield() {
    let fixture = Fixture::new();
    let long = format!("printf ok #{}", "x".repeat(300));
    for arguments in [
        json!({"command":long,"description":"print ok"}),
        json!({"command":long,"name":"long"}),
        json!({"command":long,"name":"long","description":" "}),
        json!({"command":long}),
    ] {
        let result = fixture.exec(arguments).await;
        completed(
            &result.text,
            0,
            &format!("s\n[stdout]\nok{METADATA_REMINDER}"),
        );
    }
    for (command, reminder) in [
        (format!("printf ok #{}x", "😀".repeat(144)), false),
        (format!("printf ok #{}", "😀".repeat(145)), true),
    ] {
        assert_eq!(
            command.encode_utf16().count(),
            if reminder { 301 } else { 300 }
        );
        let result = fixture.exec(json!({"command":command})).await;
        completed(
            &result.text,
            0,
            if reminder {
                METADATA_REMINDER
            } else {
                "s\n[stdout]\nok"
            },
        );
        assert_eq!(result.text.contains(METADATA_REMINDER), reminder);
    }
    let multiline = fixture
        .exec(json!({"command":"printf multi\nprintf line; exit 7"}))
        .await;
    completed(
        &multiline.text,
        7,
        &format!("s\n[stdout]\nmultiline{METADATA_REMINDER}"),
    );
    for arguments in [
        json!({"command":long,"name":"long","description":"print ok"}),
        json!({"command":"printf multi\nprintf line","name":"multiline","description":"print multiline"}),
        json!({"command":"printf short"}),
        json!({"command":"printf short","description":"print short"}),
    ] {
        assert!(
            !fixture
                .exec(arguments)
                .await
                .text
                .contains(METADATA_REMINDER)
        );
    }
    let yielded = fixture
        .exec(
            json!({"command":"printf begun;\nsleep 30","name":"metadata-yield","yield_after_ms":0}),
        )
        .await;
    assert!(
        yielded
            .text
            .starts_with("metadata-yield\nProcess output is streaming.")
    );
    assert!(yielded.text.ends_with(METADATA_REMINDER));
    assert_eq!(yielded.yielded_process.unwrap().name, "metadata-yield");
    let signal = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"metadata-yield","signal":9}),
        )
        .await;
    assert_eq!(
        signal.text,
        "Signal 9 sent to shell process 'metadata-yield'."
    );
    fixture.finish().await;
}

#[tokio::test]
async fn pipe_yield_live_paths_metadata_claim_duplicate_and_reuse_are_real_owner_state() {
    let fixture = Fixture::new();
    let command = "printf before; printf problem >&2; while [ ! -f \"$WORKDIR/release\" ]; do sleep 0.02; done; printf after";
    let initial = fixture.exec(json!({"command":command,"name":" live-pipe ","description":"stable streams","yield_after_ms":0})).await;
    let yielded = initial.yielded_process.unwrap();
    assert_eq!(yielded.name, "live-pipe");
    assert_eq!(yielded.activity_kind, ActivityKind::Execution);
    let stdout = yielded.stdout_path.as_ref().unwrap();
    let stderr = yielded.stderr_path.as_ref().unwrap();
    assert!(stdout.is_absolute() && stderr.is_absolute());
    assert!(
        stdout.starts_with(&fixture.launcher.blobs) && stderr.starts_with(&fixture.launcher.blobs)
    );
    assert_eq!(
        initial.text,
        format!(
            "live-pipe\nProcess output is streaming.\nstdout: {}\nstderr: {}",
            stdout.display(),
            stderr.display()
        )
    );
    live_text(stdout, "before").await;
    live_text(stderr, "problem").await;
    let inventory = fixture.owner.capture_inventory();
    assert_eq!(
        inventory.inventory_instance_id,
        yielded.inventory_instance_id
    );
    assert!(inventory.revision >= yielded.visible_revision);
    let state = &inventory.processes[0];
    assert_eq!(state.process_id, yielded.process_id);
    assert_eq!(state.command, command);
    assert_eq!(state.description, "stable streams");
    assert_eq!(state.origin_tool_call_id, "wrapper-call");
    assert_eq!(state.owner_agent_session_id, fixture.owner.session_id());
    assert_eq!(state.owner_agent_name, "wrapper-fixture");
    assert_eq!(state.parent_agent_session_id, "parent-session");
    assert_eq!(state.parent_agent_name, "parent");
    assert_eq!(state.depth, 1);
    assert_eq!(fixture.owner.active().await[0].name, "live-pipe");
    let duplicate = fixture
        .exec(json!({"command":"printf duplicate","name":"live-pipe"}))
        .await;
    assert_eq!(
        duplicate.text,
        "error: Shell process name 'live-pipe' is already reserved."
    );
    assert_eq!(fixture.launcher.records.lock().unwrap().len(), 1);
    let repeated = fixture
        .owner
        .claim("live-pipe")
        .await
        .unwrap()
        .wait(Some(Duration::ZERO), &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(repeated.format(), initial.text);
    assert_eq!(
        repeated.yielded_process.as_ref().unwrap().stdout_path,
        yielded.stdout_path
    );
    let claim = fixture.owner.claim("live-pipe").await.unwrap();
    fs::write(fixture.directory.0.join("release"), "").unwrap();
    let complete = tokio::time::timeout(LIMIT, claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    completed(
        &complete.format(),
        0,
        "s\n[stdout]\nbeforeafter\n[stderr]\nproblem",
    );
    assert_eq!(fs::read_to_string(stdout).unwrap(), "beforeafter");
    assert_eq!(fs::read_to_string(stderr).unwrap(), "problem");
    let reused = fixture
        .exec(json!({"command":"printf reused","name":"live-pipe"}))
        .await;
    completed(&reused.text, 0, "s\n[stdout]\nreused");
    assert!(fixture.owner.active().await.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn yielded_exec_delivers_real_completion_and_generates_an_omitted_name() {
    let fixture = Fixture::new();
    let result = fixture.exec(json!({"command":"while [ ! -f \"$WORKDIR/release\" ]; do sleep 0.02; done; printf done; exit 7","description":"background job","yield_after_ms":0})).await;
    let yielded = result.yielded_process.unwrap();
    assert!(yielded.name.starts_with("shell-"));
    assert_eq!(fixture.owner.active().await.len(), 1);
    fs::write(fixture.directory.0.join("release"), "").unwrap();
    let rows = fixture.sink.rows(1).await;
    let Row::Completion(message) = &rows[0] else {
        panic!("exec completion was emitted as monitor event")
    };
    assert!(!message.message_id.is_empty());
    assert_eq!(message.owner_agent_session_id, fixture.owner.session_id());
    assert_eq!(message.process_name, yielded.name);
    assert_eq!(
        message.activity_description,
        format!("process {} completion", yielded.name)
    );
    assert_eq!(message.delivery, Delivery::DELIVERY_STEER);
    assert_eq!(message.input_source, InputSource::INPUT_SOURCE_UNSPECIFIED);
    assert!(message.text.starts_with(&format!(
        "Shell process '{}' completed.\nProcess exited with code 7 after ",
        yielded.name
    )));
    assert!(message.text.ends_with("s\n[stdout]\ndone"));
    assert!(fixture.owner.active().await.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn qualification_routes_only_slash_names_and_signal_targets_fixture_processes() {
    let target = Fixture::new();
    let caller = Fixture::new();
    target
        .exec(json!({"command":"printf ''; sleep 30","name":"qualified-signal","yield_after_ms":0}))
        .await;
    caller.resolver.route(
        "/root/target/qualified-signal",
        &target.owner,
        "qualified-signal",
    );
    let bare = caller
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"qualified-signal","signal":9}),
        )
        .await;
    assert_eq!(
        bare.text,
        "error: Unknown shell process 'qualified-signal'."
    );
    assert!(caller.resolver.calls.lock().unwrap().is_empty());
    let claim = target.owner.claim("qualified-signal").await.unwrap();
    let signaled = caller
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":" /root/target/qualified-signal ","signal":9}),
        )
        .await;
    // The held physical claim cannot be bypassed by another owner's wrapper.
    assert_eq!(
        signaled.text,
        "error: Shell process 'qualified-signal' is already being waited for."
    );
    drop(claim);
    let signaled = caller
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"/root/target/qualified-signal","signal":9}),
        )
        .await;
    assert_eq!(
        signaled.text,
        "Signal 9 sent to shell process 'qualified-signal'."
    );
    let rows = target.sink.rows(1).await;
    let Row::Completion(message) = &rows[0] else {
        panic!("wrong completion source")
    };
    assert!(message.text.contains("Process exited with code 137 after "));
    for kind in [
        ProcessToolKind::WriteStdin,
        ProcessToolKind::InterruptProcess,
    ] {
        let result = caller
            .invoke(kind, json!({"name":"missing/process","input":""}))
            .await;
        assert_eq!(result.text, "error: resource owner unavailable");
    }
    assert_eq!(
        *caller.resolver.calls.lock().unwrap(),
        [
            "/root/target/qualified-signal",
            "/root/target/qualified-signal",
            "missing/process",
            "missing/process"
        ]
    );
    target.finish().await;
    caller.finish().await;
}

#[tokio::test]
async fn environment_survives_yield_and_the_completed_name_can_be_reused() {
    let fixture = Fixture::new();
    let initial = fixture.exec(json!({"command":"while [ ! -f \"$WORKDIR/release\" ]; do sleep 0.02; done; printf '%s' \"$LATER_VALUE\"","env":{"LATER_VALUE":"later"},"name":"later","yield_after_ms":0})).await;
    assert!(
        initial
            .text
            .starts_with("later\nProcess output is streaming.\nstdout: ")
    );
    assert!(initial.text.contains("\nstderr: "));
    let claim = fixture.owner.claim("later").await.unwrap();
    fs::write(fixture.directory.0.join("release"), "").unwrap();
    let waited = tokio::time::timeout(LIMIT, claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    completed(&waited.format(), 0, "s\n[stdout]\nlater");
    let reused = fixture
        .exec(json!({"command":"printf reused","name":"later"}))
        .await;
    completed(&reused.text, 0, "s\n[stdout]\nreused");
    assert_eq!(
        fixture.launcher.records.lock().unwrap()[0]
            .request
            .environment,
        vec![("LATER_VALUE".into(), "later".into())]
    );
    fixture.finish().await;
}

#[tokio::test]
async fn interrupt_default_two_and_explicit_seventeen_then_nine_reach_real_processes() {
    let fixture = Fixture::new();
    // A background Cargo invocation can inherit SIGINT=SIG_IGN. The native
    // launcher preserves that disposition, so a successful signal send need
    // not terminate `sleep`. Reset only this fixture child via GNU env, then
    // wait for the handler's readiness output before sending the default signal.
    let started = fixture
        .exec(json!({
            "command":r#"exec env --default-signal=INT "$T57_SIGNAL_SHELL" --noprofile --norc -c 'trap "printf interrupted; exit 42" INT; printf ready; while :; do sleep 0.02; done'"#,
            "env":{"T57_SIGNAL_SHELL":fixture.launcher.shell.to_str().unwrap()},
            "name":"default-signal",
            "yield_after_ms":0
        }))
        .await;
    let yielded = started.yielded_process.unwrap();
    live_text(yielded.stdout_path.as_ref().unwrap(), "ready").await;
    let default = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"default-signal"}),
        )
        .await;
    assert_eq!(
        default.text,
        "Signal 2 sent to shell process 'default-signal'."
    );
    let rows = fixture.sink.rows(1).await;
    let Row::Completion(message) = &rows[0] else {
        panic!("signal completion was not ordinary completion")
    };
    assert_eq!(message.process_name, "default-signal");
    assert!(message.text.contains("Process exited with code 42 after "));
    assert!(message.text.ends_with("s\n[stdout]\nreadyinterrupted"));
    fixture
        .exec(json!({"command":"printf ''; sleep 30","name":"running","yield_after_ms":0}))
        .await;
    let signal = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"running","signal":17}),
        )
        .await;
    assert_eq!(signal.text, "Signal 17 sent to shell process 'running'.");
    let killed = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"running","signal":9}),
        )
        .await;
    assert_eq!(killed.text, "Signal 9 sent to shell process 'running'.");
    let rows = fixture.sink.rows(2).await;
    let Row::Completion(message) = &rows[1] else {
        panic!("kill completion was not ordinary completion")
    };
    assert!(message.text.contains("Process exited with code 137 after "));
    let reused = fixture
        .exec(json!({"command":"printf restarted","name":"running"}))
        .await;
    completed(&reused.text, 0, "s\n[stdout]\nrestarted");
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_releases_observation_but_keeps_the_real_process_owned() {
    let fixture = Fixture::new();
    fixture
        .exec(
            json!({"command":"printf ''; sleep 30","name":"cancel-observation","yield_after_ms":0}),
        )
        .await;
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    let result = fixture
        .tool(ProcessToolKind::WriteStdin)
        .invoke(
            ProcessInvocation::new(
                "cancelled-call",
                r#"{"name":"cancel-observation","input":""}"#,
            ),
            InvocationContext::new(fixture.security.clone(), (), &cancellation),
        )
        .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert_eq!(fixture.owner.active().await.len(), 1);
    assert!(fixture.sink.rows.lock().unwrap().is_empty());
    let signal = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"cancel-observation","signal":9}),
        )
        .await;
    assert_eq!(
        signal.text,
        "Signal 9 sent to shell process 'cancel-observation'."
    );
    fixture.sink.rows(1).await;
    fixture.finish().await;
}

#[tokio::test]
async fn parallel_safety_uses_configured_read_only_prefixes_not_names_or_environment() {
    let fixture = Fixture::new();
    let tool = fixture.tool(ProcessToolKind::ExecCommand);
    assert_eq!(
        tool.parallel_safety(&ProcessInvocation::new(
            "parallel",
            r#"{"command":"git status","env":{"VALUE":"x"}}"#
        )),
        ParallelSafety::ParallelSafe
    );
    assert_eq!(
        tool.parallel_safety(&ProcessInvocation::new(
            "parallel",
            r#"{"command":"printf mutation > file"}"#
        )),
        ParallelSafety::Sequential
    );
    assert_eq!(
        tool.parallel_safety(&ProcessInvocation::new(
            "parallel",
            r#"{"command":"git status","name":" "}"#
        )),
        ParallelSafety::Sequential
    );
    assert_eq!(
        tool.parallel_safety(&ProcessInvocation::new("parallel", "[]")),
        ParallelSafety::Sequential
    );
    for kind in [
        ProcessToolKind::WriteStdin,
        ProcessToolKind::InterruptProcess,
        ProcessToolKind::Monitor,
    ] {
        assert_eq!(
            fixture
                .tool(kind)
                .parallel_safety(&ProcessInvocation::new("parallel", "{}")),
            ParallelSafety::Sequential
        );
    }
    assert!(fixture.launcher.records.lock().unwrap().is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn real_pty_stdin_and_empty_poll_consume_each_output_segment_once() {
    let fixture = Fixture::new();
    let initial = fixture.exec(json!({"command":"printf first; IFS= read -r line; printf 'received:%s' \"$line\"; exit 7","name":"interactive","yield_after_ms":500,"tty":true})).await;
    let yielded = initial.yielded_process.unwrap();
    assert!(yielded.stdout_path.is_none() && yielded.stderr_path.is_none());
    assert!(initial.text.contains("first"));
    assert!(!initial.text.contains("READY"));
    let complete = fixture
        .invoke(
            ProcessToolKind::WriteStdin,
            json!({"name":" interactive ","input":"hello\n","yield_after_ms":2000}),
        )
        .await;
    assert!(complete.yielded_process.is_none());
    assert!(
        complete
            .text
            .starts_with("Process exited with code 7 after ")
    );
    assert!(complete.text.contains("received:hello"));
    assert!(!complete.text.contains("first") && !complete.text.contains("READY"));
    assert_eq!(
        fixture.launcher.records.lock().unwrap()[0]
            .request
            .terminal_mode,
        TerminalMode::PseudoTerminal
    );

    let initial = fixture.exec(json!({"command":"printf first; while [ ! -f \"$WORKDIR/release-second\" ]; do sleep 0.02; done; printf second; sleep 30","name":"poll","yield_after_ms":500,"tty":true})).await;
    assert!(initial.text.contains("first"));
    fs::write(fixture.directory.0.join("release-second"), "").unwrap();
    let poll = fixture
        .invoke(
            ProcessToolKind::WriteStdin,
            json!({"name":"poll","input":"","yield_after_ms":500}),
        )
        .await;
    assert!(poll.yielded_process.is_none());
    assert!(poll.text.contains("second"));
    assert!(!poll.text.contains("first"));
    let signal = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"poll","signal":9}),
        )
        .await;
    assert_eq!(signal.text, "Signal 9 sent to shell process 'poll'.");
    fixture.finish().await;
}

#[tokio::test]
async fn pipe_stdin_error_preserves_the_process_claim_for_later_signal() {
    let fixture = Fixture::new();
    fixture
        .exec(json!({"command":"printf ''; sleep 30","name":"pipe-input","yield_after_ms":0}))
        .await;
    let result = fixture
        .invoke(
            ProcessToolKind::WriteStdin,
            json!({"name":"pipe-input","input":"hello","yield_after_ms":0}),
        )
        .await;
    assert_eq!(
        result.text,
        "error: Standard input is available only for pseudo-terminal processes."
    );
    assert!(result.yielded_process.is_none());
    let signal = fixture
        .invoke(
            ProcessToolKind::InterruptProcess,
            json!({"name":"pipe-input","signal":9}),
        )
        .await;
    assert_eq!(signal.text, "Signal 9 sent to shell process 'pipe-input'.");
    fixture.sink.rows(1).await;
    fixture.finish().await;
}

#[tokio::test]
async fn oversized_exec_output_is_really_spilled_into_the_fixture_blob_directory() {
    let fixture = Fixture::new();
    let result = fixture
        .exec(json!({"command":"for ((i=0;i<70000;i++)); do printf x; done"}))
        .await;
    assert!(result.text.starts_with("Process exited with code 0 after "));
    let notice = "\nTool output exceeded 64 KiB and was saved to ";
    let (_, path) = result
        .text
        .split_once(notice)
        .expect("missing real output-spill notice");
    let path = Path::new(path.strip_suffix('.').unwrap());
    assert!(path.is_absolute());
    assert_eq!(path.parent(), Some(fixture.launcher.blobs.as_path()));
    let durable = fs::read_to_string(path).unwrap();
    assert!(durable.starts_with("Process exited with code 0 after "));
    assert!(durable.ends_with(&format!("\n[stdout]\n{}", "x".repeat(70000))));
    fixture.finish().await;
    assert!(path.is_file(), "settlement must retain durable output");
}

#[tokio::test]
async fn qualified_stdin_uses_the_resolved_real_pty_owner() {
    let target = Fixture::new();
    let caller = Fixture::new();
    let initial = target.exec(json!({"command":"IFS= read -r line; printf 'qualified:%s' \"$line\"","name":"qualified-input","yield_after_ms":0,"tty":true})).await;
    assert!(initial.yielded_process.is_some());
    caller.resolver.route(
        "parent/parent/target/qualified-input",
        &target.owner,
        "qualified-input",
    );
    let result = caller.invoke(ProcessToolKind::WriteStdin, json!({"name":"parent/parent/target/qualified-input","input":"hello\n","yield_after_ms":2000})).await;
    assert!(result.text.starts_with("Process exited with code 0 after "));
    assert!(result.text.contains("qualified:hello"));
    assert!(result.yielded_process.is_none());
    assert_eq!(
        *caller.resolver.calls.lock().unwrap(),
        ["parent/parent/target/qualified-input"]
    );
    target.finish().await;
    caller.finish().await;
}

fn row_text(row: &Row) -> &str {
    match row {
        Row::Event(event) => &event.text,
        Row::Completion(message) => &message.text,
    }
}

async fn monitor_case(arguments: Value, interrupt: bool, expected: &[&str]) {
    let fixture = Fixture::new();
    let started = fixture.invoke(ProcessToolKind::Monitor, arguments).await;
    let yielded = started.yielded_process.unwrap();
    assert_eq!(yielded.name, "watch");
    assert_eq!(yielded.activity_kind, ActivityKind::Monitor);
    assert!(yielded.stdout_path.as_ref().unwrap().is_file());
    let stderr = yielded.stderr_path.as_ref().unwrap();
    assert_eq!(
        started.text,
        format!(
            "Monitor watch started. Each stdout line arrives as a notification while you keep working; a running monitor keeps this turn open. Stop it early with interrupt_process.\nstderr does not produce events and is written to {}",
            stderr.display()
        )
    );
    assert_eq!(
        fixture.owner.active().await.len(),
        1,
        "monitor must keep the turn open while running"
    );
    assert_eq!(
        fixture.launcher.records.lock().unwrap()[0]
            .request
            .terminal_mode,
        TerminalMode::Pipe
    );
    if interrupt {
        let inventory = fixture.owner.capture_inventory();
        assert_eq!(inventory.processes[0].activity_kind, ActivityKind::Monitor);
        fixture.sink.rows(1).await;
        let result = fixture
            .invoke(
                ProcessToolKind::InterruptProcess,
                json!({"name":"watch","signal":15}),
            )
            .await;
        assert_eq!(result.text, "Signal 15 sent to shell process 'watch'.");
    }
    let rows = fixture.sink.rows(expected.len()).await;
    assert_eq!(rows.iter().map(row_text).collect::<Vec<_>>(), expected);
    for row in &rows[..rows.len() - 1] {
        let Row::Event(event) = row else {
            panic!("monitor event was sent as ordinary completion")
        };
        assert_eq!(event.source, MonitorInputSource::Monitor);
        assert_eq!(event.delivery, MonitorDelivery::Steer);
        assert_eq!(event.activity_name, "watch");
    }
    let Row::Completion(completion) = rows.last().unwrap() else {
        panic!("monitor terminal row was not ordinary completion")
    };
    assert_eq!(completion.delivery, Delivery::DELIVERY_STEER);
    assert_eq!(
        completion.input_source,
        InputSource::INPUT_SOURCE_UNSPECIFIED
    );
    assert_eq!(completion.process_name, "watch");
    assert!(fixture.sink.failures.lock().unwrap().is_empty());
    assert!(fixture.owner.active().await.is_empty());
    fixture.finish().await;
    assert_eq!(fixture.sink.rows.lock().unwrap().len(), expected.len());
}

#[tokio::test]
async fn monitor_streams_stdout_batches_not_stderr_then_reports_exit() {
    monitor_case(json!({"command":"printf 'a\\nb\\n'; sleep 1; echo c; echo hidden >&2; exit 3","description":"ticks","name":"watch"}), false, &[
        "Monitor watch (ticks):\na\nb",
        "Monitor watch (ticks):\nc",
        "Monitor watch (ticks) exited with code 3. Events delivered: 3.",
    ]).await;
}

#[tokio::test]
async fn monitor_interrupt_reports_real_signal_exit() {
    monitor_case(json!({"command":"echo ready; while true; do sleep 0.1; done","description":"ready","name":"watch"}), true, &[
        "Monitor watch (ready):\nready",
        "Monitor watch (ready) exited with code 143. Events delivered: 1.",
    ]).await;
}

#[tokio::test]
async fn monitor_silence_reaches_timeout_and_is_stopped() {
    monitor_case(json!({"command":"while true; do sleep 0.1; done","description":"idle","name":"watch","timeout_ms":1000}), false, &[
        "Monitor watch (idle) reached its 1000ms timeout and was stopped; start it again if you still need the watch. Events delivered: 0.",
    ]).await;
}

#[tokio::test]
async fn monitor_flood_is_stopped_without_delivering_the_flood() {
    monitor_case(json!({"command":"seq 1 500; while true; do sleep 0.1; done","description":"flood","name":"watch"}), false, &[
        "Monitor watch (flood) produced too many events and was stopped; start it again with a tighter filter. Events delivered: 0.",
    ]).await;
}

#[tokio::test]
async fn monitor_environment_identifier_checks_follow_the_source_wrapper_checks() {
    let fixture = Fixture::new();
    for (arguments, expected) in [
        (
            json!({"command":"","env":{"VALUE":"bad\u{0}value"}}),
            "error: no command given",
        ),
        (
            json!({"command":"true","env":{"":"x"}}),
            "error: description must not be empty",
        ),
        (
            json!({"command":"true","description":"d","name":" ","env":{"":"x"}}),
            "error: process name must not be empty",
        ),
        (
            json!({"command":"true","description":"d","timeout_ms":999,"env":{"":"x"}}),
            "error: Tool argument 'timeout_ms' must be between 1000 and 1800000.",
        ),
    ] {
        assert_eq!(
            fixture
                .invoke(ProcessToolKind::Monitor, arguments)
                .await
                .text,
            expected
        );
    }
    for environment in [
        json!({"":"x"}),
        json!({"INVALID=NAME":"x"}),
        json!({"VALUE":"bad\u{0}value"}),
    ] {
        let result = fixture
            .invoke(
                ProcessToolKind::Monitor,
                json!({"command":"true","description":"d","env":environment}),
            )
            .await;
        assert_eq!(
            result.text,
            "error: Tool argument 'env' contains an invalid environment value."
        );
    }
    assert!(fixture.launcher.records.lock().unwrap().is_empty());
    assert!(fixture.owner.capture_inventory().processes.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn cancelling_an_active_exec_wait_releases_claim_without_detaching_ownership() {
    let fixture = Fixture::new();
    let cancellation = ProcessLifetime::default();
    let tool = fixture.tool(ProcessToolKind::ExecCommand);
    let wait = tool.invoke(
        ProcessInvocation::new("cancel-active", json!({"command":"while [ ! -f \"$WORKDIR/release-active\" ]; do sleep 0.01; done; printf survived","name":"active-wait"}).to_string()),
        InvocationContext::new(fixture.security.clone(), (), &cancellation),
    );
    let cancel = async {
        tokio::time::timeout(LIMIT, async {
            while fixture.owner.capture_inventory().processes.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(wait, cancel);
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert_eq!(fixture.owner.active().await.len(), 1);
    let claim = fixture.owner.claim("active-wait").await.unwrap();
    fs::write(fixture.directory.0.join("release-active"), "").unwrap();
    let outcome = tokio::time::timeout(LIMIT, claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    completed(&outcome.format(), 0, "s\n[stdout]\nsurvived");
    fixture.finish().await;
}

#[tokio::test]
async fn precancelled_exec_keeps_the_started_process_owned_until_observed() {
    let fixture = Fixture::new();
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    let result = fixture.tool(ProcessToolKind::ExecCommand).invoke(
        ProcessInvocation::new("precancelled", json!({"command":"while [ ! -f \"$WORKDIR/release-pre\" ]; do sleep 0.01; done; printf complete","name":"precancelled"}).to_string()),
        InvocationContext::new(fixture.security.clone(), (), &cancellation),
    ).await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    assert_eq!(fixture.owner.active().await.len(), 1);
    let claim = fixture.owner.claim("precancelled").await.unwrap();
    fs::write(fixture.directory.0.join("release-pre"), "").unwrap();
    let outcome = tokio::time::timeout(LIMIT, claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    completed(&outcome.format(), 0, "s\n[stdout]\ncomplete");
    fixture.finish().await;
}

#[tokio::test]
async fn precancelled_interrupt_does_not_signal_or_consume_the_owner() {
    let fixture = Fixture::new();
    fixture.exec(json!({"command":"while true; do sleep 0.05; done","name":"no-signal","yield_after_ms":0})).await;
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    let result = fixture
        .tool(ProcessToolKind::InterruptProcess)
        .invoke(
            ProcessInvocation::new("cancel-signal", r#"{"name":"no-signal","signal":9}"#),
            InvocationContext::new(fixture.security.clone(), (), &cancellation),
        )
        .await;
    assert!(matches!(result, Err(OperationError::Cancelled)));
    let claim = fixture.owner.claim("no-signal").await.unwrap();
    assert!(!claim.process().snapshot().unwrap().completed);
    claim.send_signal(9, &NeverCancelled).unwrap();
    fixture.sink.rows(1).await;
    fixture.finish().await;
}

#[tokio::test]
async fn monitor_ignores_the_invocation_token_and_owns_a_separate_report() {
    let fixture = Fixture::new();
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    let result = fixture.tool(ProcessToolKind::Monitor).invoke(
        ProcessInvocation::new("cancel-monitor", r#"{"command":"printf 'token-independent\\n'","description":"own lifetime","name":"token-watch"}"#),
        InvocationContext::new(fixture.security.clone(), (), &cancellation),
    ).await.unwrap();
    assert_eq!(
        result.yielded_process.unwrap().activity_kind,
        ActivityKind::Monitor
    );
    let rows = fixture.sink.rows(2).await;
    assert_eq!(
        row_text(&rows[0]),
        "Monitor token-watch (own lifetime):\ntoken-independent"
    );
    assert_eq!(
        row_text(&rows[1]),
        "Monitor token-watch (own lifetime) exited with code 0. Events delivered: 1."
    );
    fixture.finish().await;
}

#[tokio::test]
async fn invocation_error_templates_format_parser_errors() {
    let fixture = Fixture::new();
    let (_, templates) = PromptTemplateCatalog::from_content(
        "prompt_templates:\n  tool-result.error:\n    template: 'custom-error<{message}>'\n",
        &BTreeMap::from([(
            "HOME".into(),
            fixture.directory.0.to_string_lossy().into_owned(),
        )]),
    )
    .unwrap();
    let mut invocation = ProcessInvocation::new("custom-error", "{}");
    invocation.prompt_templates = Some(Arc::new(templates));
    let result = fixture
        .tool(ProcessToolKind::ExecCommand)
        .invoke(
            invocation,
            InvocationContext::new(fixture.security.clone(), (), &NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(
        result.text,
        "custom-error<Tool arguments require a string 'command'.>"
    );
    assert!(fixture.launcher.records.lock().unwrap().is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn startup_failures_are_returned_without_fallback_and_do_not_reserve_names() {
    let fixture = Fixture::new();
    *fixture.launcher.failure.lock().unwrap() =
        Some((io::ErrorKind::Unsupported, "sandbox unavailable".into()));
    let result = fixture
        .exec(json!({"command":"printf must-not-run","name":"failed-start"}))
        .await;
    assert_eq!(result.text, "error: sandbox unavailable");
    assert!(result.yielded_process.is_none());
    assert_eq!(fixture.launcher.records.lock().unwrap().len(), 1);
    assert!(fixture.launcher.executions.lock().unwrap().is_empty());
    assert!(fixture.owner.capture_inventory().processes.is_empty());
    *fixture.launcher.failure.lock().unwrap() = Some((
        io::ErrorKind::Interrupted,
        "native startup interrupted".into(),
    ));
    let result = fixture
        .exec(json!({"command":"true","name":"failed-start"}))
        .await;
    assert_eq!(result.text, "error: native startup interrupted");
    *fixture.launcher.failure.lock().unwrap() = None;
    let result = fixture
        .exec(json!({"command":"printf reusable","name":"failed-start"}))
        .await;
    completed(&result.text, 0, "s\n[stdout]\nreusable");
    fixture.finish().await;
}

#[tokio::test]
async fn completed_pty_spilled_suffix_survives_incremental_transcript_cleanup() {
    let fixture = Fixture::new();
    let initial = fixture.exec(json!({"command":"printf prefix; while [ ! -f \"$WORKDIR/release-spill\" ]; do sleep 0.01; done; for ((i=0;i<70000;i++)); do printf x; done","name":"spill","tty":true,"yield_after_ms":100})).await;
    assert!(initial.text.contains("prefix"));
    assert!(initial.yielded_process.is_some());
    let claim = fixture.owner.claim("spill").await.unwrap();
    fs::write(fixture.directory.0.join("release-spill"), "").unwrap();
    let outcome = tokio::time::timeout(LIMIT, claim.wait(None, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    let result = outcome.result.unwrap();
    let path = result.blob_path.unwrap();
    let durable = fs::read_to_string(&path).unwrap();
    assert!(!durable.contains("prefix"));
    let (_, stdout) = durable.split_once("[stdout]\n").unwrap();
    assert_eq!(stdout.len(), 70000);
    assert!(stdout.bytes().all(|byte| byte == b'x'));
    fixture.finish().await;
    assert!(path.is_file());
    assert!(fs::read_dir(&fixture.launcher.blobs).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".process-")
    }));
}
