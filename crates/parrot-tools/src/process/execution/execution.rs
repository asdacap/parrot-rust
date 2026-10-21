//! Owned execution/output composition, without sandbox selection or authorization.
//!
//! One finite supervisor and one drain per native stream belong to this object.
//! Drop cancels, reaps the direct child, and joins them. Process groups are not
//! universal descendant containment. Pollable drains can stop even when an
//! unrelated process keeps an inherited output descriptor open.

use super::LaunchSpec;
use super::filenames::HaikunatorNameGenerator;
use super::native::NativeProcess;
use super::output::{
    self, MAX_OUTPUT_CHARACTERS, PipeProcessOutputFiles, ProcessOutput, ProcessOutputBlobStore,
    Utf8Decoder, Utf16Writer,
};
use super::pipe_decoder::PipeDecoder;
use super::transcript::PtyTranscript;
use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::filenames::FilenameGenerator;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessResult {
    pub exit_code: i32,
    pub elapsed_milliseconds: i64,
    pub stdout: String,
    pub stderr: String,
    pub blob_path: Option<PathBuf>,
}
impl ProcessResult {
    pub fn format(&self) -> String {
        output::format_result(
            self.exit_code,
            self.elapsed_milliseconds,
            &self.stdout,
            &self.stderr,
            self.blob_path.as_deref(),
        )
    }
}

#[derive(Clone, Debug)]
struct BlobIoError {
    directory: PathBuf,
    source: Arc<io::Error>,
}
impl std::fmt::Display for BlobIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Process output in blob directory '{}': {}",
            self.directory.display(),
            self.source
        )
    }
}
impl std::error::Error for BlobIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
fn blob_error(directory: &Path, error: io::Error) -> io::Error {
    if error
        .get_ref()
        .is_some_and(|inner| inner.is::<BlobIoError>())
    {
        return error;
    }
    io::Error::new(
        error.kind(),
        BlobIoError {
            directory: directory.to_path_buf(),
            source: Arc::new(error),
        },
    )
}

#[derive(Clone)]
struct Failure(io::ErrorKind, String, Option<i32>, Option<BlobIoError>);
impl Failure {
    fn capture(error: &io::Error) -> Self {
        Self(
            error.kind(),
            error.to_string(),
            error.raw_os_error(),
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<BlobIoError>())
                .cloned(),
        )
    }
    fn error(&self) -> io::Error {
        if let Some(context) = &self.3 {
            return io::Error::new(self.0, context.clone());
        }
        self.2.map_or_else(
            || io::Error::new(self.0, self.1.clone()),
            io::Error::from_raw_os_error,
        )
    }
}

struct Shared {
    native: Mutex<Option<NativeProcess>>,
    cancelled: AtomicBool,
    drains_cancelled: AtomicBool,
    failure: Mutex<Option<Failure>>,
    exited: Mutex<Option<(ExitStatus, i64)>>,
}
impl Shared {
    fn stop(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(native) = recover(&self.native).as_mut() {
            if let Err(error) = native.request_cancel() {
                let mut failure = recover(&self.failure);
                if failure.is_none() {
                    *failure = Some(Failure::capture(&error));
                }
            }
        }
    }
    fn fail(&self, error: &io::Error) {
        {
            let mut failure = recover(&self.failure);
            if failure.is_none() {
                *failure = Some(Failure::capture(error));
            }
        }
        self.stop();
    }
}

#[derive(Default)]
struct State {
    drains: Vec<JoinHandle<io::Result<ProcessOutput>>>,
    result: Option<Result<ProcessResult, Failure>>,
}

struct CancellationWatch {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl CancellationWatch {
    fn join(self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        self.thread.join().map_err(|_| {
            io::Error::other("The process cancellation observer panicked during cleanup.")
        })
    }
}

struct CleanupPath(Option<PathBuf>);

impl CleanupPath {
    fn remove(&mut self) -> io::Result<()> {
        if let Some(path) = self.0.take() {
            match std::fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// A real process execution, not a security policy. Completion progresses without
/// observers. Cancelling a wait future leaves the process owned by this object.
/// No mutex guard is held across an await. Supervisor handles are outside the
/// shared state captured by that thread, so ownership does not form a cycle.
pub struct ShellProcessExecution {
    shared: Arc<Shared>,
    state: Arc<Mutex<State>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    cancellation_watch: Mutex<Option<CancellationWatch>>,
    cleanup_path: Mutex<CleanupPath>,
    files: Option<Arc<Mutex<PipeProcessOutputFiles>>>,
    transcript: Option<Arc<PtyTranscript>>,
    blob_directory: PathBuf,
    writing: AtomicBool,
    id: u32,
}

impl ShellProcessExecution {
    /// Transfer the private launch profile before attempting process startup.
    /// The profile survives Result completion and is deleted only on settlement
    /// or Drop, after native/output resources have been released.
    pub fn start_pipe_with_cleanup_path(
        spec: &LaunchSpec,
        blob_directory: impl AsRef<Path>,
        cleanup_path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        let mut owned_path = CleanupPath(Some(cleanup_path.into()));
        match Self::start_pipe(spec, blob_directory) {
            Ok(execution) => {
                recover(&execution.cleanup_path).0 = owned_path.0.take();
                Ok(execution)
            }
            Err(error) => {
                owned_path.remove()?;
                Err(error)
            }
        }
    }

    /// Link the caller's cancellation to this owned execution rather than to a
    /// particular wait future. The finite observer is joined during cleanup.
    pub fn bind_cancellation(&mut self, cancellation: Arc<dyn Cancellation>) -> io::Result<()> {
        let mut observer = recover(&self.cancellation_watch);
        if observer.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The process already has a cancellation lifetime.",
            ));
        }
        if cancellation.is_cancelled() {
            self.shared.stop();
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let shared = self.shared.clone();
        let thread = thread::Builder::new()
            .name("parrot-process-cancellation".into())
            .spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    if cancellation.is_cancelled() {
                        shared.stop();
                        break;
                    }
                    thread::park_timeout(TICK);
                }
            })?;
        *observer = Some(CancellationWatch { stop, thread });
        Ok(())
    }

    pub fn start_pipe(spec: &LaunchSpec, blob_directory: impl AsRef<Path>) -> io::Result<Self> {
        let mut files = PipeProcessOutputFiles::open(blob_directory.as_ref())
            .map_err(|error| blob_error(blob_directory.as_ref(), error))?;
        let started = Instant::now();
        let native = match NativeProcess::start_pipe(spec) {
            Ok(native) => native,
            Err(error) => {
                let _ = files.delete_startup_artifacts();
                return Err(error);
            }
        };
        let files = Arc::new(Mutex::new(files));
        let execution = Self::new(native, blob_directory.as_ref(), Some(files.clone()), None);
        let startup = (|| {
            for stdout in [true, false] {
                let reader = {
                    let mut native = recover(&execution.shared.native);
                    let native = native.as_mut().expect("new execution owns child");
                    if stdout {
                        Reader::Stdout(native.take_stdout().expect("pipe stdout"))
                    } else {
                        Reader::Stderr(native.take_stderr().expect("pipe stderr"))
                    }
                };
                let shared = execution.shared.clone();
                let files = files.clone();
                execution.spawn_drain(move || drain_pipe(reader, files, stdout, &shared))?;
            }
            execution.start_supervisor(started)
        })();
        if let Err(error) = startup {
            drop(execution);
            let _ = recover(&files).delete_startup_artifacts();
            return Err(error);
        }
        Ok(execution)
    }

    pub async fn start_pty(
        spec: &LaunchSpec,
        helper: &Path,
        blob_directory: impl AsRef<Path>,
        rows: u16,
        columns: u16,
    ) -> io::Result<Self> {
        let started = Instant::now();
        let native = NativeProcess::start_pty(spec, helper, rows, columns).await?;
        let transcript = Arc::new(PtyTranscript::new(blob_directory.as_ref()));
        let execution = Self::new(
            native,
            blob_directory.as_ref(),
            None,
            Some(transcript.clone()),
        );
        let (reader, stderr) = {
            let mut native = recover(&execution.shared.native);
            let native = native.as_mut().expect("new execution owns child");
            (
                native.take_pty_reader().expect("PTY reader"),
                native.take_stderr().expect("bridge stderr"),
            )
        };
        let shared = execution.shared.clone();
        execution.spawn_drain(move || {
            let result = read_bytes(reader, &shared, |bytes| transcript.append(bytes));
            let completion = transcript.complete();
            result.and(completion).map(|()| ProcessOutput::inline(""))
        })?;
        let shared = execution.shared.clone();
        let directory = execution.blob_directory.clone();
        execution.spawn_drain(move || drain_diagnostics(stderr, &directory, &shared))?;
        execution.start_supervisor(started)?;
        Ok(execution)
    }

    fn new(
        native: NativeProcess,
        directory: &Path,
        files: Option<Arc<Mutex<PipeProcessOutputFiles>>>,
        transcript: Option<Arc<PtyTranscript>>,
    ) -> Self {
        let id = native.id();
        Self {
            shared: Arc::new(Shared {
                native: Mutex::new(Some(native)),
                cancelled: AtomicBool::new(false),
                drains_cancelled: AtomicBool::new(false),
                failure: Mutex::new(None),
                exited: Mutex::new(None),
            }),
            state: Arc::new(Mutex::new(State::default())),
            supervisor: Mutex::new(None),
            cancellation_watch: Mutex::new(None),
            cleanup_path: Mutex::new(CleanupPath(None)),
            files,
            transcript,
            blob_directory: directory.to_path_buf(),
            writing: AtomicBool::new(false),
            id,
        }
    }

    fn start_supervisor(&self, started: Instant) -> io::Result<()> {
        let shared = self.shared.clone();
        let state = self.state.clone();
        let transcript = self.transcript.clone();
        let directory = self.blob_directory.clone();
        let handle = thread::Builder::new()
            .name("parrot-process-execution".into())
            .spawn(move || {
                loop {
                    if shared.native.is_poisoned() || state.is_poisoned() {
                        shared.fail(&io::Error::other("The execution state lock was poisoned."));
                    }
                    let needs_exit = recover(&shared.exited).is_none();
                    if needs_exit {
                        let status = {
                            let mut native = recover(&shared.native);
                            native.as_mut().map(NativeProcess::try_wait).transpose()
                        };
                        match status {
                            Ok(Some(Some(status))) => {
                                let elapsed =
                                    started.elapsed().as_millis().min(i64::MAX as u128) as i64;
                                *recover(&shared.exited) = Some((status, elapsed));
                            }
                            Ok(_) => (),
                            Err(error) => shared.fail(&error),
                        }
                    }
                    if shared.cancelled.load(Ordering::Acquire) {
                        shared.stop();
                        let native = recover(&shared.native).take();
                        drop(native);
                        // The owned producer is now killed/reaped. Each reader
                        // preserves a finite snapshot of bytes already queued,
                        // without waiting for unrelated inherited writers.
                        shared.drains_cancelled.store(true, Ordering::Release);
                        for output in join_drains(take_drains(&state)).into_iter().flatten() {
                            if let Err(error) = output.delete_temporary_file() {
                                shared.fail(&blob_error(&directory, error));
                            }
                        }
                        let error = recover(&shared.failure)
                            .clone()
                            .unwrap_or_else(|| Failure::capture(&cancelled()));
                        recover(&state).result = Some(Err(error));
                        return;
                    }
                    let finished = recover(&state).drains.iter().all(JoinHandle::is_finished);
                    let exited = recover(&shared.exited).is_some();
                    if exited && finished {
                        let outputs = join_drains(take_drains(&state));
                        let result = finish(&shared, transcript.as_deref(), &directory, outputs);
                        recover(&state).result =
                            Some(result.map_err(|error| Failure::capture(&error)));
                        return;
                    }
                    thread::sleep(TICK);
                }
            })?;
        *recover(&self.supervisor) = Some(handle);
        Ok(())
    }

    fn spawn_drain(
        &self,
        drain: impl FnOnce() -> io::Result<ProcessOutput> + Send + 'static,
    ) -> io::Result<()> {
        let shared = self.shared.clone();
        let directory = self.blob_directory.clone();
        let handle = thread::Builder::new()
            .name("parrot-process-output".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(drain))
                    .unwrap_or_else(|_| Err(io::Error::other("The output drain panicked.")))
                    .map_err(|error| {
                        if error.kind() == io::ErrorKind::Interrupted
                            && shared.cancelled.load(Ordering::Acquire)
                        {
                            error
                        } else {
                            blob_error(&directory, error)
                        }
                    });
                if let Err(error) = &result {
                    if error.kind() != io::ErrorKind::Interrupted
                        || !shared.cancelled.load(Ordering::Acquire)
                    {
                        shared.fail(error);
                    }
                }
                result
            })?;
        recover(&self.state).drains.push(handle);
        Ok(())
    }

    pub fn id(&self) -> u32 {
        self.id
    }
    /// True only after the child and all drains have settled into a cached result.
    pub fn is_completed(&self) -> bool {
        recover(&self.state).result.is_some()
    }
    pub fn try_result(&self) -> io::Result<Option<ProcessResult>> {
        recover(&self.state)
            .result
            .clone()
            .transpose()
            .map_err(|error| error.error())
    }
    pub fn is_pseudo_terminal(&self) -> bool {
        self.transcript.is_some()
    }
    pub fn stdout_path(&self) -> Option<PathBuf> {
        self.files
            .as_ref()
            .map(|files| recover(files).stdout_path().to_path_buf())
    }
    pub fn stderr_path(&self) -> Option<PathBuf> {
        self.files
            .as_ref()
            .map(|files| recover(files).stderr_path().to_path_buf())
    }
    pub fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        if let Some(transcript) = &self.transcript {
            return transcript.read(offset);
        }
        if offset != 0 {
            return Err(output::invalid("Pipe transcript offset must be zero."));
        }
        let files = recover(self.files.as_ref().expect("pipe files"));
        Ok((
            0,
            format!(
                "stdout: {}\nstderr: {}\n",
                files.stdout_path().display(),
                files.stderr_path().display()
            )
            .encode_utf16()
            .collect(),
        ))
    }
    pub async fn wait(&self) -> io::Result<ProcessResult> {
        loop {
            if let Some(result) = self.try_result()? {
                return Ok(result);
            }
            tokio::time::sleep(TICK).await;
        }
    }
    pub fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)> {
        let completed = self.try_result()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "The process has not completed.")
        })?;
        if offset == 0 {
            return Ok((self.transcript.as_ref().map_or(0, |t| t.len()), completed));
        }
        let transcript = self.transcript.as_ref().ok_or_else(|| {
            output::invalid("Incremental results are available only for pseudo-terminal processes.")
        })?;
        let (cursor, stdout) = transcript.read_output(offset)?;
        let result = format_result(
            &self.blob_directory,
            completed.exit_code,
            completed.elapsed_milliseconds,
            &stdout,
            &ProcessOutput::inline(""),
            None,
        );
        let _ = stdout.delete_temporary_file();
        result.map(|result| (cursor, result))
    }

    pub async fn write_stdin(&self, input: &str) -> io::Result<()> {
        if !self.is_pseudo_terminal() {
            return Err(output::invalid(
                "Standard input is available only for pseudo-terminal processes.",
            ));
        }
        loop {
            if self.shared.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            if self
                .writing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(TICK).await;
        }
        let _gate = WriteGate(&self.writing);
        let mut remaining = input.as_bytes();
        // Even an empty write must reject a completed/disposed terminal.
        loop {
            if self.shared.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            let written = recover(&self.shared.native)
                .as_mut()
                .ok_or_else(cancelled)?
                .try_write_stdin(remaining);
            match written {
                Ok(count) if count > 0 => remaining = &remaining[count..],
                Ok(_) if remaining.is_empty() => return Ok(()),
                Ok(_) => tokio::time::sleep(TICK).await,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(TICK).await
                }
                Err(error) => return Err(error),
            }
            if remaining.is_empty() {
                return Ok(());
            }
        }
    }
    pub fn resize(&self, rows: u16, columns: u16) -> io::Result<()> {
        recover(&self.shared.native)
            .as_ref()
            .ok_or_else(cancelled)?
            .resize(rows, columns)
    }
    pub fn send_signal(&self, signal: i32) -> io::Result<()> {
        recover(&self.shared.native)
            .as_ref()
            .ok_or_else(cancelled)?
            .send_signal(signal)
    }
    pub fn request_cancel(&self) {
        if self.is_completed() {
            return;
        }
        self.shared.stop();
    }
    pub async fn settle(&self) -> io::Result<()> {
        self.request_cancel();
        // Source AwaitCompletionForDisposal observes and suppresses prior work
        // failure, but launcher/resource cleanup is a separate error boundary.
        // Joining the supervisor also handles a faulted supervisor without an
        // indefinitely pending result observation.
        self.cleanup()
    }
    fn cleanup(&self) -> io::Result<()> {
        let mut first = None;
        self.shared.stop();
        // Serialize the complete synchronous cleanup, including leftover drain
        // joins and transcript disposal if the supervisor itself failed.
        let mut supervisor = recover(&self.supervisor);
        if let Some(observer) = recover(&self.cancellation_watch).take() {
            if let Err(error) = observer.join() {
                first = Some(error);
            }
        }
        if let Some(supervisor) = supervisor.take() {
            if supervisor.join().is_err() {
                let error = io::Error::other("The process supervisor panicked during cleanup.");
                let mut state = recover(&self.state);
                if state.result.is_none() {
                    let failure = recover(&self.shared.failure)
                        .clone()
                        .unwrap_or_else(|| Failure::capture(&error));
                    state.result = Some(Err(failure));
                }
                first.get_or_insert(error);
            }
        }
        // Also covers partially constructed executions with no supervisor.
        let native = recover(&self.shared.native).take();
        drop(native);
        self.shared.drains_cancelled.store(true, Ordering::Release);
        for drain in take_drains(&self.state) {
            match drain.join() {
                Ok(Ok(output)) => {
                    if let Err(error) = output.delete_temporary_file() {
                        first.get_or_insert_with(|| blob_error(&self.blob_directory, error));
                    }
                }
                // A prior drain failure belongs to the work result, not cleanup.
                Ok(Err(_)) => (),
                Err(_) => {
                    first.get_or_insert_with(|| {
                        io::Error::other("The output drain panicked during cleanup.")
                    });
                }
            }
        }
        if let Some(transcript) = &self.transcript {
            if let Err(error) = transcript.dispose() {
                first.get_or_insert_with(|| blob_error(&self.blob_directory, error));
            }
        }
        if let Some(files) = &self.files {
            let mut files = recover(files);
            for result in [files.complete_stdout(), files.complete_stderr()] {
                if let Err(error) = result {
                    first.get_or_insert_with(|| blob_error(&self.blob_directory, error));
                }
            }
        }
        if let Err(error) = recover(&self.cleanup_path).remove() {
            first.get_or_insert(error);
        }
        first.map_or(Ok(()), Err)
    }
}
impl Drop for ShellProcessExecution {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn finish(
    shared: &Shared,
    transcript: Option<&PtyTranscript>,
    directory: &Path,
    outputs: Vec<io::Result<ProcessOutput>>,
) -> io::Result<ProcessResult> {
    let mut actual = Vec::new();
    let mut first = recover(&shared.failure)
        .clone()
        .map(|failure| failure.error());
    for output in outputs {
        match output {
            Ok(output) => actual.push(output),
            Err(error) if first.is_none() => first = Some(error),
            Err(_) => (),
        }
    }
    let mut result = (|| {
        if let Some(error) = first {
            return Err(error);
        }
        let (status, elapsed) = recover(&shared.exited).ok_or_else(|| {
            io::Error::other("The process supervisor ended without an exit status.")
        })?;
        let exit = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
        if let Some(transcript) = transcript {
            let diagnostics = actual
                .get(1)
                .ok_or_else(|| io::Error::other("Missing bridge diagnostics."))?;
            if !diagnostics.is_empty() {
                let text = if diagnostics.spilled() {
                    format!(
                        "{} UTF-16 units (diagnostics exceed inline limit)",
                        diagnostics.len()
                    )
                } else {
                    diagnostics.text().trim_end().to_owned()
                };
                return Err(io::Error::other(format!(
                    "The PTY bridge reported an error: {text}"
                )));
            }
            let (_, stdout) = transcript.read_output(0)?;
            format_result(
                directory,
                exit,
                elapsed,
                &stdout,
                &ProcessOutput::inline(""),
                None,
            )
        } else {
            if actual.len() != 2 {
                return Err(io::Error::other("Missing pipe output."));
            }
            format_result(
                directory,
                exit,
                elapsed,
                &actual[0],
                &actual[1],
                Some(&shared.cancelled),
            )
        }
    })();
    for output in actual {
        if let Err(error) = output.delete_temporary_file() {
            if result.is_ok() {
                result = Err(blob_error(directory, error));
            }
        }
    }
    result
}
fn format_result(
    directory: &Path,
    exit: i32,
    elapsed: i64,
    stdout: &ProcessOutput,
    stderr: &ProcessOutput,
    cancellation: Option<&AtomicBool>,
) -> io::Result<ProcessResult> {
    let mut result = ProcessResult {
        exit_code: exit,
        elapsed_milliseconds: elapsed,
        stdout: stdout.text(),
        stderr: stderr.text(),
        blob_path: None,
    };
    if stdout.spilled() || stderr.spilled() || result.format().len() > MAX_OUTPUT_CHARACTERS {
        let names = HaikunatorNameGenerator::default();
        result.blob_path = Some(
            ProcessOutputBlobStore::new(directory)
                .persist_with(
                    exit,
                    elapsed,
                    stdout,
                    stderr,
                    || names.next_filename(),
                    || cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)),
                )
                .map_err(|error| blob_error(directory, error))?,
        );
        result.stdout.clear();
        result.stderr.clear();
    }
    Ok(result)
}
fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poison does not discard ownership. Cleanup must still reap/join resources.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn take_drains(state: &Mutex<State>) -> Vec<JoinHandle<io::Result<ProcessOutput>>> {
    std::mem::take(&mut recover(state).drains)
}
fn join_drains(
    drains: Vec<JoinHandle<io::Result<ProcessOutput>>>,
) -> Vec<io::Result<ProcessOutput>> {
    drains
        .into_iter()
        .map(|thread| {
            thread
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("The output drain panicked.")))
        })
        .collect()
}
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "The operation was canceled.")
}
struct WriteGate<'a>(&'a AtomicBool);
impl Drop for WriteGate<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum Reader {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
}
impl AsRawFd for Reader {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Stdout(r) => r.as_raw_fd(),
            Self::Stderr(r) => r.as_raw_fd(),
        }
    }
}
fn read_bytes(
    reader: impl AsRawFd,
    shared: &Shared,
    mut append: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = [0u8; 4096];
    loop {
        if shared.drains_cancelled.load(Ordering::Acquire) {
            drain_available(fd, &mut append)?;
            return Err(cancelled());
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        match count.cmp(&0) {
            std::cmp::Ordering::Greater => append(&buffer[..count as usize])?,
            std::cmp::Ordering::Equal => return Ok(()),
            std::cmp::Ordering::Less => {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EIO) {
                    return Ok(());
                }
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error);
            }
        }
    }
}
// Snapshot only the bytes queued after the owned child has been killed/reaped.
// A separately owned writer may remain open or keep producing; its later bytes
// cannot make cancellation/Drop wait forever. No arbitrary byte cap is applied.
fn drain_available(
    fd: std::os::fd::RawFd,
    append: &mut impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut available: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EIO) {
            return Ok(());
        }
        return Err(error);
    }
    let mut remaining = available.max(0) as usize;
    let mut buffer = [0u8; 4096];
    while remaining > 0 {
        let count =
            unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), remaining.min(buffer.len())) };
        match count.cmp(&0) {
            std::cmp::Ordering::Greater => {
                append(&buffer[..count as usize])?;
                remaining -= count as usize;
            }
            std::cmp::Ordering::Equal => break,
            std::cmp::Ordering::Less => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(libc::EIO)
                {
                    break;
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

fn drain_pipe(
    reader: Reader,
    files: Arc<Mutex<PipeProcessOutputFiles>>,
    stdout: bool,
    shared: &Shared,
) -> io::Result<ProcessOutput> {
    let directory = recover(&files)
        .stdout_path()
        .parent()
        .expect("absolute output path")
        .to_path_buf();
    let mut decoder = PipeDecoder::default();
    let mut inline = Vec::new();
    let mut spill: Option<PrivateOutput> = None;
    let mut append = |bytes: &[u8], complete: bool| -> io::Result<()> {
        let units = decoder.append(bytes, complete);
        {
            let mut files = recover(&files);
            if stdout {
                files.append_stdout_utf16(&units)?;
            } else {
                files.append_stderr_utf16(&units)?;
            }
        }
        if let Some(spill) = &mut spill {
            spill.append(&units)?;
        } else if inline.len() + units.len() <= MAX_OUTPUT_CHARACTERS {
            inline.extend_from_slice(&units);
        } else {
            // Source Spill maintains an independent private copy, not a reread
            // of public live output files that callers can overwrite or unlink.
            let mut private = PrivateOutput::new(&directory)?;
            private.append(&inline)?;
            private.append(&units)?;
            inline.clear();
            spill = Some(private);
        }
        Ok(())
    };
    let result = read_bytes(reader, shared, |bytes| append(bytes, false));
    let reached_end = result.is_ok()
        || result.as_ref().is_err_and(|error| {
            error.kind() == io::ErrorKind::Interrupted
                && shared.drains_cancelled.load(Ordering::Acquire)
        });
    let flushed = if reached_end {
        append(&[], true)
    } else {
        Ok(())
    };
    let completion = {
        let mut files = recover(&files);
        if stdout {
            files.complete_stdout()
        } else {
            files.complete_stderr()
        }
    };
    result.and(flushed).and(completion)?;
    match spill {
        Some(spill) => spill.finish(),
        None => Ok(ProcessOutput::inline_utf16(inline)),
    }
}

/// A private streamed copy that is removed on all error/unwind paths. Successful
/// ownership is handed to ProcessOutput and the final formatter deletes it.
struct PrivateOutput {
    path: PathBuf,
    file: Option<std::fs::File>,
    encoder: Utf16Writer,
    handed_off: bool,
}
impl PrivateOutput {
    fn new(directory: &Path) -> io::Result<Self> {
        let (path, file) = output::temporary_file(directory)?;
        Ok(Self {
            path,
            file: Some(file),
            encoder: Utf16Writer::default(),
            handed_off: false,
        })
    }
    fn append(&mut self, units: &[u16]) -> io::Result<()> {
        self.encoder
            .append(self.file.as_mut().expect("open private output"), units)
    }
    fn finish(mut self) -> io::Result<ProcessOutput> {
        self.encoder
            .complete(self.file.as_mut().expect("open private output"))?;
        self.file.take();
        let output = ProcessOutput::from_utf8_file(self.path.clone(), true)?;
        self.handed_off = true;
        Ok(output)
    }
}
impl Drop for PrivateOutput {
    fn drop(&mut self) {
        self.file.take();
        if !self.handed_off {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn drain_diagnostics(
    reader: impl AsRawFd,
    directory: &Path,
    shared: &Shared,
) -> io::Result<ProcessOutput> {
    let mut private = PrivateOutput::new(directory)?;
    let mut decoder = Utf8Decoder::default();
    let mut inline = Vec::new();
    let mut spilled = false;
    read_bytes(reader, shared, |bytes| {
        let units = decoder.append(bytes, false);
        private.append(&units)?;
        if !spilled {
            if inline.len() + units.len() <= MAX_OUTPUT_CHARACTERS {
                inline.extend_from_slice(&units);
            } else {
                spilled = true;
                inline.clear();
            }
        }
        Ok(())
    })?;
    let units = decoder.append(&[], true);
    private.append(&units)?;
    if !spilled {
        if inline.len() + units.len() <= MAX_OUTPUT_CHARACTERS {
            inline.extend_from_slice(&units);
        } else {
            spilled = true;
            inline.clear();
        }
    }
    if spilled {
        private.finish()
    } else {
        Ok(ProcessOutput::inline_utf16(inline))
    }
}

#[cfg(test)]
#[path = "../../../tests/process/execution/composition.rs"]
mod tests;
