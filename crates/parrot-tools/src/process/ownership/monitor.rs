//! Stdout-only monitor observations over an owned process execution.
//!
//! Execution ownership, agent delivery, configured templates and blob storage are
//! injected. This observer neither starts a process nor provides authorization.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Cancelled, Clock};
use parrot_domain::protocol::messages::parrot::ShellProcessActivityKind;

use crate::process::execution::output::Utf8Decoder;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const FLOOD_WINDOW: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
const FLOOD_LINES: usize = 100;
const MAX_INLINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorIdentity {
    pub name: String,
    pub description: String,
    pub owner_agent_session_id: String,
    pub process_id: String,
}

/// The already-owned execution. Completion must not be detached by an observer.
pub trait MonitorExecution: Send + Sync {
    fn stdout_path(&self) -> Option<PathBuf>;
    fn is_completed(&self) -> bool;
    fn send_signal(&self, signal: i32) -> io::Result<()>;
    fn completion(&self) -> BoxFuture<'_, io::Result<i32>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorReason {
    Exited,
    TimedOut,
    Flooded,
    Failed,
}

impl MonitorReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::TimedOut => "timed_out",
            Self::Flooded => "flooded",
            Self::Failed => "failed",
        }
    }
}

/// Exact arguments supplied to the configured `monitor.ended` template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorReport {
    pub events: usize,
    pub reason: MonitorReason,
    pub exit_code: String,
    pub error: String,
    pub timeout_ms: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorInputSource {
    Monitor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorDelivery {
    Steer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorEvent {
    pub text: String,
    pub activity_name: String,
    pub activity_description: String,
    pub source: MonitorInputSource,
    pub delivery: MonitorDelivery,
}

/// Agent/template/blob dependencies, without a runtime or host implementation import.
/// The owner sends the terminal completion separately with its ordinary source;
/// only streamed events use `MonitorInputSource::Monitor`.
pub trait MonitorSink: Send + Sync {
    fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String>;
    fn persist_event<'a>(
        &'a self,
        text: String,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<String>>;
    /// Delivers one Steer input with a fresh message identity and Monitor source.
    fn send_event<'a>(
        &'a self,
        event: MonitorEvent,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>>;
    /// Records monitor/observe/failed with owner/process correlation and error
    /// classification, not event text or a raw-output diagnostic field.
    fn observe_failed(&self, identity: &MonitorIdentity, failure: &io::Error);
    fn render_completion(
        &self,
        identity: &MonitorIdentity,
        report: &MonitorReport,
    ) -> io::Result<String>;
}

pub struct ShellProcessMonitor {
    timeout: Duration,
    report: MonitorReport,
}

impl ShellProcessMonitor {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            report: MonitorReport {
                events: 0,
                reason: MonitorReason::Exited,
                exit_code: String::new(),
                error: String::new(),
                timeout_ms: timeout.as_millis().to_string(),
            },
        }
    }

    pub fn blocks_turn(&self) -> bool {
        false
    }

    pub fn activity_kind(&self) -> ShellProcessActivityKind {
        ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
    }

    pub fn report(&self) -> &MonitorReport {
        &self.report
    }

    pub fn complete(
        &self,
        identity: &MonitorIdentity,
        sink: &dyn MonitorSink,
    ) -> io::Result<String> {
        sink.render_completion(identity, &self.report)
    }

    pub async fn observe(
        &mut self,
        identity: &MonitorIdentity,
        execution: &dyn MonitorExecution,
        sink: &dyn MonitorSink,
        lifetime: &dyn Cancellation,
    ) {
        self.observe_with_clock(identity, execution, sink, &SystemMonitorClock, lifetime)
            .await;
    }

    /// The injected monotonic clock is also used by real file-observer tests;
    /// no simulated process result is inferred from a clock tick.
    pub async fn observe_with_clock(
        &mut self,
        identity: &MonitorIdentity,
        execution: &dyn MonitorExecution,
        sink: &dyn MonitorSink,
        clock: &dyn Clock,
        lifetime: &dyn Cancellation,
    ) {
        tokio::task::yield_now().await;
        let observed = self
            .stream(identity, execution, sink, clock, lifetime)
            .await;
        let result = match observed {
            Ok(()) => execution.completion().await,
            Err(error) => Err(error),
        };
        match result {
            Ok(code) => self.report.exit_code = code.to_string(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted && lifetime.is_cancelled() => {
            }
            Err(error) => {
                self.report.reason = MonitorReason::Failed;
                self.report.error = error.to_string();
                sink.observe_failed(identity, &error);
                signal(execution, 9);
            }
        }
    }

    async fn stream(
        &mut self,
        identity: &MonitorIdentity,
        execution: &dyn MonitorExecution,
        sink: &dyn MonitorSink,
        clock: &dyn Clock,
        lifetime: &dyn Cancellation,
    ) -> io::Result<()> {
        let path = execution.stdout_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "A monitor requires pipe output.",
            )
        })?;
        let mut reader = LiveStdout::new(File::open(path)?);
        let mut pending = String::new();
        let started = clock.now();
        let mut state = PollState::new(started);
        loop {
            let completed = execution.is_completed();
            pending.push_str(&reader.read_new(completed, lifetime)?);
            let (lines, count) = take_lines(&mut pending, completed);
            if count > 0 && self.report.reason != MonitorReason::Flooded {
                let now = clock.now();
                if now.duration_since(state.window_started) > FLOOD_WINDOW {
                    state.window_started = now;
                    state.window_lines = 0;
                }
                state.window_lines += count;
                if state.window_lines > FLOOD_LINES && state.stopped.is_none() {
                    self.report.reason = MonitorReason::Flooded;
                    signal(execution, 15);
                    state.stopped = Some(clock.now());
                } else {
                    self.send(identity, lines, count, sink, lifetime).await?;
                }
            }
            // Source returns on completion before timeout/escalation checks.
            if completed {
                return Ok(());
            }
            let now = clock.now();
            if state.stopped.is_none() && now.duration_since(started) >= self.timeout {
                self.report.reason = MonitorReason::TimedOut;
                signal(execution, 15);
                state.stopped = Some(clock.now());
            }
            if let Some(stopped) = state.stopped {
                if !state.killed && clock.now().duration_since(stopped) >= KILL_GRACE {
                    signal(execution, 9);
                    state.killed = true;
                }
            }
            clock
                .sleep_until(clock.now() + POLL_INTERVAL, lifetime)
                .await
                .map_err(|_| cancelled())?;
        }
    }

    async fn send(
        &mut self,
        identity: &MonitorIdentity,
        lines: String,
        count: usize,
        sink: &dyn MonitorSink,
        lifetime: &dyn Cancellation,
    ) -> io::Result<()> {
        let mut text = sink.render_event(identity, &lines)?;
        if text.len() > MAX_INLINE_BYTES {
            text = sink.persist_event(text, lifetime).await?;
        }
        sink.send_event(
            MonitorEvent {
                text,
                activity_name: identity.name.clone(),
                activity_description: format!("monitor {} event", identity.name),
                source: MonitorInputSource::Monitor,
                delivery: MonitorDelivery::Steer,
            },
            lifetime,
        )
        .await?;
        self.report.events += count;
        Ok(())
    }
}

struct PollState {
    window_started: Instant,
    window_lines: usize,
    stopped: Option<Instant>,
    killed: bool,
}

impl PollState {
    fn new(started: Instant) -> Self {
        Self {
            window_started: started,
            window_lines: 0,
            stopped: None,
            killed: false,
        }
    }
}

fn take_lines(pending: &mut String, all: bool) -> (String, usize) {
    let end = if all {
        pending.len()
    } else {
        pending.rfind('\n').map_or(0, |index| index + 1)
    };
    let lines = pending[..end].trim_end_matches('\n').to_owned();
    pending.drain(..end);
    let count = if lines.is_empty() {
        0
    } else {
        lines.bytes().filter(|byte| *byte == b'\n').count() + 1
    };
    (lines, count)
}

fn signal(execution: &dyn MonitorExecution, value: i32) {
    // Source suppresses IO, invalid-state and unavailable-platform signal errors:
    // completion is still observed even when a racing exit makes a signal fail.
    let _ = execution.send_signal(value);
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "The operation was canceled.")
}

struct LiveStdout {
    file: File,
    decoder: Utf8Decoder,
    initial_text: bool,
}

impl LiveStdout {
    fn new(file: File) -> Self {
        Self {
            file,
            decoder: Utf8Decoder::default(),
            initial_text: true,
        }
    }

    fn read_new(&mut self, complete: bool, lifetime: &dyn Cancellation) -> io::Result<String> {
        let mut text = String::new();
        let mut buffer = [0; 4096];
        loop {
            lifetime.check().map_err(|_| cancelled())?;
            let count = self.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            text.push_str(&String::from_utf16_lossy(
                &self.decoder.append(&buffer[..count], false),
            ));
        }
        if complete {
            text.push_str(&String::from_utf16_lossy(&self.decoder.append(&[], true)));
        }
        if self.initial_text && !text.is_empty() {
            // StreamReader's initial UTF-8 BOM detection, not embedded-BOM removal.
            if text.starts_with('\u{feff}') {
                text.drain(..'\u{feff}'.len_utf8());
            }
            self.initial_text = false;
        }
        Ok(text)
    }
}

struct SystemMonitorClock;

impl Clock for SystemMonitorClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            lifetime.check()?;
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Ok(()),
                _ = lifetime.cancelled() => Err(Cancelled),
            }
        })
    }
}

#[cfg(test)]
#[path = "../../../tests/process/ownership/monitor.rs"]
mod tests;
