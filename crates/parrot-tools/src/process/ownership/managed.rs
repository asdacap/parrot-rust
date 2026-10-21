//! Exclusive observations and completion delivery of one stable process instance.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::protocol::messages::parrot::{Delivery, InputSource};
use parrot_domain::values::identifiers::Identifier;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;

use super::execution::{Execution, Failure, NeverCancelled, ProcessLifetime, ProcessResult};
use super::inventory::{
    ActiveShellProcessState, ActivityKind, ShellProcessInventory, YieldedShellProcess,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionMessage {
    pub message_id: String,
    pub owner_agent_session_id: String,
    pub process_name: String,
    pub text: String,
    pub activity_description: String,
    pub delivery: Delivery,
    pub input_source: InputSource,
}

/// A completion is a steer input with unspecified input source, not a monitor event.
pub trait CompletionSink: Send + Sync {
    fn send<'a>(
        &'a self,
        message: &'a CompletionMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessDiagnostic {
    pub operation: &'static str,
    pub outcome: &'static str,
    pub owner_agent_session_id: String,
    pub process_id: String,
    pub elapsed_milliseconds: u64,
    pub error_kind: Option<io::ErrorKind>,
}

pub trait ProcessDiagnostics: Send + Sync {
    fn write(&self, event: ProcessDiagnostic);
}

pub struct NoProcessDiagnostics;
impl ProcessDiagnostics for NoProcessDiagnostics {
    fn write(&self, _: ProcessDiagnostic) {}
}

pub trait ProcessReport: Send + Sync {
    fn blocks_turn(&self) -> bool;
    fn activity_kind(&self) -> ActivityKind;
    fn observe<'a>(
        &'a self,
        state: &'a ActiveShellProcessState,
        execution: Arc<dyn Execution>,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>>;
    fn complete(&self, state: &ActiveShellProcessState, output: &str) -> io::Result<String>;
}

pub struct CompletionReport;
impl ProcessReport for CompletionReport {
    fn blocks_turn(&self) -> bool {
        true
    }
    fn activity_kind(&self) -> ActivityKind {
        ActivityKind::Execution
    }
    fn observe<'a>(
        &'a self,
        _: &'a ActiveShellProcessState,
        _: Arc<dyn Execution>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn complete(&self, state: &ActiveShellProcessState, output: &str) -> io::Result<String> {
        Ok(format!(
            "Shell process '{}' completed.\n{output}",
            state.name
        ))
    }
}

/// Current state of this physical managed instance, without claiming it or
/// advancing its output cursor.
#[derive(Clone, Debug)]
pub struct ManagedProcessSnapshot {
    pub process: ActiveShellProcessState,
    pub completed: bool,
    pub retired: bool,
    pub stdout_path: Option<std::path::PathBuf>,
    pub stderr_path: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ShellWaitResult {
    pub name: String,
    pub running: bool,
    pub output: Vec<u16>,
    pub result: Option<ProcessResult>,
    pub yielded_process: Option<YieldedShellProcess>,
}

impl ShellWaitResult {
    pub fn format(&self) -> String {
        if let Some(result) = &self.result {
            return result.format();
        }
        if let Some(yielded) = &self.yielded_process {
            if let (Some(stdout), Some(stderr)) = (&yielded.stdout_path, &yielded.stderr_path) {
                return format!(
                    "{}\nProcess output is streaming.\nstdout: {}\nstderr: {}",
                    self.name,
                    stdout.display(),
                    stderr.display()
                );
            }
        }
        let text = String::from_utf16_lossy(&self.output);
        if text.is_empty() {
            self.name.clone()
        } else if text.ends_with('\n') {
            format!("{text}{}", self.name)
        } else {
            format!("{text}\n{}", self.name)
        }
    }
}

#[derive(Default)]
struct State {
    claimed: bool,
    retired: bool,
    cursor: u64,
    completed: Option<Result<(), Failure>>,
    observation: Option<Result<(), Failure>>,
}

#[derive(Default)]
struct Settlement {
    tasks: Vec<JoinHandle<io::Result<()>>>,
    failure: Option<Failure>,
    result: Option<Result<(), Failure>>,
}

pub struct ManagedShellProcess {
    pub state: ActiveShellProcessState,
    execution: Mutex<Option<Arc<dyn Execution>>>,
    inventory: Arc<ShellProcessInventory>,
    report: Arc<dyn ProcessReport>,
    sink: Arc<dyn CompletionSink>,
    diagnostics: Arc<dyn ProcessDiagnostics>,
    lifetime: Arc<ProcessLifetime>,
    visible: YieldedShellProcess,
    inner: Mutex<State>,
    changed: Notify,
    settlement: AsyncMutex<Settlement>,
    retirement: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl ManagedShellProcess {
    pub(crate) fn start(
        state: ActiveShellProcessState,
        execution: Arc<dyn Execution>,
        inventory: Arc<ShellProcessInventory>,
        report: Arc<dyn ProcessReport>,
        sink: Arc<dyn CompletionSink>,
        diagnostics: Arc<dyn ProcessDiagnostics>,
        lifetime: Arc<ProcessLifetime>,
    ) -> io::Result<ClaimedProcess> {
        let visible = inventory.publish(state.clone()).map_err(io::Error::other)?;
        let process = Arc::new(Self {
            state,
            execution: Mutex::new(Some(execution)),
            inventory,
            report,
            sink,
            diagnostics,
            lifetime,
            visible,
            inner: Mutex::new(State {
                claimed: true,
                ..State::default()
            }),
            changed: Notify::new(),
            settlement: AsyncMutex::new(Settlement::default()),
            retirement: Mutex::new(None),
        });
        // Construction is synchronous: no caller can settle between spawning and
        // installing these handles. A dropped observation never drops a handle.
        let observe = Arc::clone(&process);
        let delivery = Arc::clone(&process);
        let tasks = vec![
            tokio::spawn(async move { observe.observe().await }),
            tokio::spawn(async move { delivery.deliver_when_unclaimed().await }),
        ];
        process
            .settlement
            .try_lock()
            .expect("new process settlement")
            .tasks = tasks;
        Ok(ClaimedProcess {
            process,
            active: true,
        })
    }

    pub fn name(&self) -> &str {
        &self.state.name
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.lifetime.is_cancelled()
    }
    pub fn snapshot(&self) -> io::Result<ManagedProcessSnapshot> {
        let state = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("The process state lock is poisoned."))?;
        let execution = self
            .execution
            .lock()
            .map_err(|_| io::Error::other("The process execution lock is poisoned."))?;
        Ok(ManagedProcessSnapshot {
            process: self.state.clone(),
            completed: state.completed.is_some(),
            retired: state.retired,
            stdout_path: execution.as_ref().and_then(|process| process.stdout_path()),
            stderr_path: execution.as_ref().and_then(|process| process.stderr_path()),
        })
    }
    pub fn completed(&self) -> bool {
        self.inner
            .lock()
            .expect("process state")
            .completed
            .is_some()
    }
    pub fn retired(&self) -> bool {
        self.inner.lock().expect("process state").retired
    }
    pub fn blocks_turn(&self) -> bool {
        self.report.blocks_turn()
    }
    pub fn request_cancel(&self) {
        let execution = self.execution.lock().expect("process execution").clone();
        if let Some(execution) = execution {
            execution.request_cancel();
        }
    }

    fn execution(&self) -> io::Result<Arc<dyn Execution>> {
        self.execution
            .lock()
            .expect("process execution")
            .clone()
            .ok_or_else(|| delivered(self.name()))
    }

    pub fn claim(self: &Arc<Self>) -> io::Result<ClaimedProcess> {
        let mut state = self.inner.lock().expect("process state");
        if state.retired {
            return Err(delivered(self.name()));
        }
        if state.claimed {
            return Err(io::Error::other(format!(
                "Shell process '{}' is already being waited for.",
                self.name()
            )));
        }
        state.claimed = true;
        Ok(ClaimedProcess {
            process: Arc::clone(self),
            active: true,
        })
    }

    fn yielded(&self) -> YieldedShellProcess {
        let mut yielded = self.visible.clone();
        let execution = self.execution().ok();
        yielded.stdout_path = execution
            .as_ref()
            .and_then(|execution| execution.stdout_path());
        yielded.stderr_path = execution
            .as_ref()
            .and_then(|execution| execution.stderr_path());
        yielded
    }

    fn diagnostic(
        &self,
        operation: &'static str,
        outcome: &'static str,
        error: Option<&io::Error>,
    ) {
        self.diagnostics.write(ProcessDiagnostic {
            operation,
            outcome,
            owner_agent_session_id: self.state.owner_agent_session_id.clone(),
            process_id: self.state.process_id.clone(),
            elapsed_milliseconds: elapsed(self.state.started),
            error_kind: error.map(io::Error::kind),
        });
    }

    async fn observe(self: Arc<Self>) -> io::Result<()> {
        let execution = self.execution()?;
        let completion = async {
            let result = tokio::select! {
                result = execution.wait() => result,
                () = self.lifetime.cancelled() => {
                    execution.request_cancel();
                    execution.wait().await
                }
            };
            let duration = result
                .as_ref()
                .ok()
                .map(|result| result.elapsed_milliseconds);
            match &result {
                Ok(result) => self.diagnostic(
                    "completed",
                    if result.exit_code == 0 {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    None,
                ),
                Err(error) => self.diagnostic(
                    "completed",
                    if error.kind() == io::ErrorKind::Interrupted {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    Some(error),
                ),
            }
            self.inventory.complete(&self.state.process_id, duration);
            self.inner.lock().expect("process state").completed =
                Some(result.map(|_| ()).map_err(|error| Failure::capture(&error)));
            self.changed.notify_waiters();
        };
        let observation = async {
            let result = self
                .report
                .observe(&self.state, Arc::clone(&execution), self.lifetime.as_ref())
                .await;
            self.inner.lock().expect("process state").observation =
                Some(result.as_ref().map(|_| ()).map_err(Failure::capture));
            self.changed.notify_waiters();
            result
        };
        let ((), observed) = tokio::join!(completion, observation);
        observed
    }

    async fn changed_until(&self, predicate: impl Fn(&State) -> bool) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if predicate(&self.inner.lock().expect("process state")) {
                return;
            }
            changed.await;
        }
    }

    fn release_claim(&self) {
        let mut state = self.inner.lock().expect("process state");
        if state.claimed && !state.retired {
            state.claimed = false;
            drop(state);
            self.changed.notify_waiters();
        }
    }

    fn retire_locked(&self, state: &mut State) {
        if state.retired {
            return;
        }
        state.retired = true;
        state.claimed = false;
        let mut retained = self.execution.lock().expect("process execution");
        if let Some(execution) = retained.as_ref().cloned() {
            if execution.stdout_path().is_some() {
                // Pipe output lives in its files: retired history must not keep
                // the execution's cached strings or native resources alive.
                retained.take();
            }
            let retirement = tokio::spawn(async move { execution.settle().await });
            *self.retirement.lock().expect("process retirement") = Some(retirement);
        }
        self.changed.notify_waiters();
    }

    fn commit_completed(&self) -> io::Result<ShellWaitResult> {
        let mut state = self.inner.lock().expect("process state");
        if state.retired {
            return Err(delivered(self.name()));
        }
        if let Some(Err(error)) = &state.completed {
            let error = error.error();
            self.retire_locked(&mut state);
            return Err(error);
        }
        let read = self.execution()?.read_result(state.cursor);
        match read {
            Ok((cursor, result)) => {
                state.cursor = cursor;
                self.retire_locked(&mut state);
                Ok(ShellWaitResult {
                    name: self.name().into(),
                    running: false,
                    output: Vec::new(),
                    result: Some(result),
                    yielded_process: None,
                })
            }
            Err(error) => {
                self.retire_locked(&mut state);
                Err(error)
            }
        }
    }

    fn commit_running(&self) -> io::Result<ShellWaitResult> {
        let mut state = self.inner.lock().expect("process state");
        if state.completed.is_some() {
            drop(state);
            return self.commit_completed();
        }
        let (cursor, output) = self.execution()?.read_transcript(state.cursor)?;
        let yielded = self.yielded();
        state.cursor = cursor;
        state.claimed = false;
        drop(state);
        self.changed.notify_waiters();
        self.diagnostic("yield", "running", None);
        Ok(ShellWaitResult {
            name: self.name().into(),
            running: true,
            output,
            result: None,
            yielded_process: Some(yielded),
        })
    }

    async fn deliver_when_unclaimed(self: Arc<Self>) -> io::Result<()> {
        self.changed_until(|state| state.completed.is_some()).await;
        let failed = {
            let state = self.inner.lock().expect("process state");
            match state.completed.as_ref().expect("completed") {
                Ok(_) => None,
                Err(error) if error.kind == io::ErrorKind::Interrupted => return Ok(()),
                Err(error) => Some(format!("error: {}", error.message)),
            }
        };
        self.changed_until(|state| state.observation.is_some())
            .await;
        {
            let mut state = self.inner.lock().expect("process state");
            if let Some(Err(error)) = &state.observation {
                let error = error.error();
                self.retire_locked(&mut state);
                return Err(error);
            }
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.inner.lock().expect("process state");
                if state.retired {
                    return Ok(());
                }
                if !state.claimed {
                    state.claimed = true;
                    break;
                }
            }
            tokio::select! {
                () = changed => {},
                () = self.lifetime.cancelled() => return Ok(()),
            }
        }
        let mut speculative: Option<(u64, u64, ProcessResult)> = None;
        let delivery = async {
            let output = if let Some(error) = failed {
                error
            } else {
                let state = self.inner.lock().expect("process state");
                let start_cursor = state.cursor;
                let (cursor, result) = self.execution()?.read_result(start_cursor)?;
                let output = result.format();
                speculative = Some((start_cursor, cursor, result));
                output
            };
            let text = self.report.complete(&self.state, &output)?;
            let message = CompletionMessage {
                message_id: Identifier::message_id()?,
                owner_agent_session_id: self.state.owner_agent_session_id.clone(),
                process_name: self.name().into(),
                text,
                activity_description: format!("process {} completion", self.name()),
                delivery: Delivery::DELIVERY_STEER,
                input_source: InputSource::INPUT_SOURCE_UNSPECIFIED,
            };
            if self
                .sink
                .send(&message, self.lifetime.as_ref())
                .await
                .is_err()
            {
                self.sink.send(&message, &NeverCancelled).await?;
            }
            let mut state = self.inner.lock().expect("process state");
            if let Some((start, cursor, _)) = &speculative {
                if state.cursor != *start {
                    return Err(io::Error::other(format!(
                        "Shell process '{}' output was consumed concurrently.",
                        self.name()
                    )));
                }
                state.cursor = *cursor;
            }
            self.retire_locked(&mut state);
            Ok(())
        }
        .await;
        if let Err(error) = &delivery {
            if let Some((start, _, result)) = speculative {
                if start > 0 {
                    if let Some(path) = result.blob_path {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
            if error.kind() == io::ErrorKind::Interrupted && self.lifetime.is_cancelled() {
                self.release_claim();
            } else {
                self.diagnostic("delivery", "failed", Some(error));
                let mut state = self.inner.lock().expect("process state");
                self.retire_locked(&mut state);
            }
        }
        // The source report loop records terminal delivery failures and retires
        // the process; those handled failures do not fail owner settlement.
        Ok(())
    }

    /// All handles stay stored while awaited, including if an observer drops this future.
    pub async fn settle(&self) -> io::Result<()> {
        let mut settlement = self.settlement.lock().await;
        if let Some(result) = &settlement.result {
            return result.as_ref().map(|_| ()).map_err(Failure::error);
        }
        while let Some(task) = settlement.tasks.last_mut() {
            if let Err(error) = join(task).await {
                settlement
                    .failure
                    .get_or_insert_with(|| Failure::capture(&error));
            }
            settlement.tasks.pop();
        }
        // Both drivers have finished creating retirement work. Keep its handle
        // stored before awaiting so a dropped settle observer cannot detach it.
        if let Some(retirement) = self.retirement.lock().expect("process retirement").take() {
            settlement.tasks.push(retirement);
        }
        while let Some(task) = settlement.tasks.last_mut() {
            if let Err(error) = join(task).await {
                settlement
                    .failure
                    .get_or_insert_with(|| Failure::capture(&error));
            }
            settlement.tasks.pop();
        }
        let execution = self.execution.lock().expect("process execution").clone();
        if let Some(execution) = execution {
            if let Err(error) = execution.settle().await {
                settlement
                    .failure
                    .get_or_insert_with(|| Failure::capture(&error));
            }
        }
        let result = settlement
            .failure
            .as_ref()
            .map_or(Ok(()), |error| Err(error.error()));
        settlement.result = Some(result.as_ref().map(|_| ()).map_err(Failure::capture));
        result
    }
}

/// A claim belongs to this instance, never a fresh lookup of its reusable name.
pub struct ClaimedProcess {
    process: Arc<ManagedShellProcess>,
    active: bool,
}
impl ClaimedProcess {
    pub fn process(&self) -> &Arc<ManagedShellProcess> {
        &self.process
    }
    pub fn yield_now(mut self) -> YieldedShellProcess {
        let visible = self.process.yielded();
        self.process.release_claim();
        self.active = false;
        visible
    }
    pub async fn wait(
        mut self,
        yield_after: Option<Duration>,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ShellWaitResult> {
        cancellation.check().map_err(|_| cancelled())?;
        let completion = self
            .process
            .changed_until(|state| state.completed.is_some());
        let completed = if let Some(delay) = yield_after {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(cancelled()),
                () = completion => true,
                () = tokio::time::sleep(delay) => false,
            }
        } else {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(cancelled()),
                () = completion => true,
            }
        };
        let result = if completed {
            self.process.commit_completed()
        } else {
            self.process.commit_running()
        };
        if result.is_err() {
            let mut state = self.process.inner.lock().expect("process state");
            self.process.retire_locked(&mut state);
        }
        self.active = false;
        result
    }
    pub async fn write_stdin(
        self,
        input: &str,
        yield_after: Duration,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ShellWaitResult> {
        cancellation.check().map_err(|_| cancelled())?;
        let execution = self.process.execution()?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(cancelled()),
            result = execution.write_stdin(input) => result?,
        }
        self.wait(Some(yield_after), cancellation).await
    }
    pub fn send_signal(self, signal: i32, cancellation: &dyn Cancellation) -> io::Result<()> {
        let result = cancellation
            .check()
            .map_err(|_| cancelled())
            .and_then(|()| self.process.execution()?.send_signal(signal));
        let outcome = match &result {
            Ok(()) => "sent",
            Err(error) if error.kind() == io::ErrorKind::Interrupted => "cancelled",
            Err(_) => "failed",
        };
        self.process
            .diagnostic("signal", outcome, result.as_ref().err());
        result
    }
}
impl Drop for ClaimedProcess {
    fn drop(&mut self) {
        if self.active {
            self.process.release_claim();
        }
    }
}

fn delivered(name: &str) -> io::Error {
    io::Error::other(format!(
        "Shell process '{name}' has already been delivered."
    ))
}
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")
}
fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}
async fn join(task: &mut JoinHandle<io::Result<()>>) -> io::Result<()> {
    task.await.map_err(io::Error::other)?
}
