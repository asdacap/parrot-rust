use super::*;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Lifetime(AtomicBool);
impl Cancellation for Lifetime {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}

struct Execution {
    path: Option<PathBuf>,
    completed: AtomicBool,
    elapsed: Arc<AtomicU64>,
    signals: Mutex<Vec<(u64, i32)>>,
    exit_code: i32,
    signal_error: bool,
    completion_error: bool,
}
impl MonitorExecution for Execution {
    fn stdout_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }
    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }
    fn send_signal(&self, signal: i32) -> io::Result<()> {
        self.signals
            .lock()
            .unwrap()
            .push((self.elapsed.load(Ordering::SeqCst), signal));
        if self.signal_error {
            Err(io::Error::new(io::ErrorKind::Unsupported, "already exited"))
        } else {
            Ok(())
        }
    }
    fn completion(&self) -> BoxFuture<'_, io::Result<i32>> {
        Box::pin(async move {
            assert!(self.is_completed());
            if self.completion_error {
                Err(io::Error::other("execution failed"))
            } else {
                Ok(self.exit_code)
            }
        })
    }
}

#[derive(Default)]
struct Sink {
    events: Mutex<Vec<MonitorEvent>>,
    persisted: Mutex<Vec<String>>,
    failures: Mutex<Vec<(MonitorIdentity, String)>>,
    completions: Mutex<Vec<MonitorReport>>,
    send_error: bool,
    persist_error: bool,
}
impl MonitorSink for Sink {
    fn render_event(&self, identity: &MonitorIdentity, lines: &str) -> io::Result<String> {
        // Exact default monitor.event template read from pinned configuration.
        Ok(format!(
            "Monitor {} ({}):\n{}",
            identity.name, identity.description, lines
        ))
    }
    fn persist_event<'a>(
        &'a self,
        text: String,
        _lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<String>> {
        Box::pin(async move {
            self.persisted.lock().unwrap().push(text);
            if self.persist_error {
                Err(io::Error::other("persist failed"))
            } else {
                Ok("Tool output exceeded 64 KiB and was saved to /fixture/blob.".into())
            }
        })
    }
    fn send_event<'a>(
        &'a self,
        event: MonitorEvent,
        _lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if self.send_error {
                Err(io::Error::other("delivery failed"))
            } else {
                self.events.lock().unwrap().push(event);
                Ok(())
            }
        })
    }
    fn observe_failed(&self, identity: &MonitorIdentity, error: &io::Error) {
        self.failures
            .lock()
            .unwrap()
            .push((identity.clone(), error.to_string()));
    }
    fn render_completion(
        &self,
        identity: &MonitorIdentity,
        report: &MonitorReport,
    ) -> io::Result<String> {
        self.completions.lock().unwrap().push(report.clone());
        // Test adapter for the actual monitor.ended template. Production injects
        // the configured renderer; this does not add an English fallback there.
        let ending = match report.reason {
            MonitorReason::Exited => format!("exited with code {}", report.exit_code),
            MonitorReason::TimedOut => format!(
                "reached its {}ms timeout and was stopped; start it again if you still need the watch",
                report.timeout_ms
            ),
            MonitorReason::Flooded => {
                "produced too many events and was stopped; start it again with a tighter filter"
                    .into()
            }
            MonitorReason::Failed => format!("failed: {}", report.error),
        };
        Ok(format!(
            "Monitor {} ({}) {}. Events delivered: {}.",
            identity.name, identity.description, ending, report.events
        ))
    }
}

struct Fixture {
    directory: PathBuf,
    execution: Execution,
    lifetime: Lifetime,
    identity: MonitorIdentity,
}
impl Fixture {
    fn new(bytes: &[u8], completed: bool) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "parrot-monitor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("stdout");
        fs::write(&path, bytes).unwrap();
        // This distinct stream must never be read by the monitor.
        fs::write(directory.join("stderr"), "hidden\n").unwrap();
        Self {
            directory,
            execution: Execution {
                path: Some(path),
                completed: AtomicBool::new(completed),
                elapsed: Arc::new(AtomicU64::new(0)),
                signals: Mutex::new(Vec::new()),
                exit_code: 3,
                signal_error: false,
                completion_error: false,
            },
            lifetime: Lifetime::default(),
            identity: MonitorIdentity {
                name: "watch".into(),
                description: "ticks".into(),
                owner_agent_session_id: "agent-id".into(),
                process_id: "process-id".into(),
            },
        }
    }
    async fn run(&self, timeout: Duration, steps: Vec<Step>, sink: &Sink) -> ShellProcessMonitor {
        let clock = ScriptClock {
            base: Instant::now(),
            fixture: self,
            steps: Mutex::new(steps.into()),
        };
        let mut monitor = ShellProcessMonitor::new(timeout);
        monitor
            .observe_with_clock(
                &self.identity,
                &self.execution,
                sink,
                &clock,
                &self.lifetime,
            )
            .await;
        monitor
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

struct Step {
    advance_ms: u64,
    append: Vec<u8>,
    complete: bool,
    cancel: bool,
}
impl Step {
    fn new(advance_ms: u64, append: impl Into<Vec<u8>>, complete: bool) -> Self {
        Self {
            advance_ms,
            append: append.into(),
            complete,
            cancel: false,
        }
    }
}
struct ScriptClock<'a> {
    base: Instant,
    fixture: &'a Fixture,
    steps: Mutex<VecDeque<Step>>,
}
impl Clock for ScriptClock<'_> {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.fixture.execution.elapsed.load(Ordering::SeqCst))
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            lifetime.check()?;
            assert_eq!(deadline.duration_since(self.now()), POLL_INTERVAL);
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("script ended before process completion");
            self.fixture
                .execution
                .elapsed
                .fetch_add(step.advance_ms, Ordering::SeqCst);
            let mut file = OpenOptions::new()
                .append(true)
                .open(self.fixture.execution.path.as_ref().unwrap())
                .unwrap();
            file.write_all(&step.append).unwrap();
            file.flush().unwrap();
            if step.complete {
                self.fixture
                    .execution
                    .completed
                    .store(true, Ordering::SeqCst);
            }
            if step.cancel {
                self.fixture.lifetime.0.store(true, Ordering::SeqCst);
            }
            lifetime.check()
        })
    }
}

fn lines(count: usize) -> Vec<u8> {
    "line\n".repeat(count).into_bytes()
}

#[test]
fn source_take_lines_preserves_cr_and_only_trims_trailing_lf() {
    let mut pending = "a\n\nb\n\npartial".to_owned();
    assert_eq!(take_lines(&mut pending, false), ("a\n\nb".into(), 3));
    assert_eq!(pending, "partial");
    assert_eq!(take_lines(&mut pending, false), (String::new(), 0));
    assert_eq!(take_lines(&mut pending, true), ("partial".into(), 1));
    assert!(pending.is_empty());
    assert_eq!(take_lines(&mut "\n\n".into(), true), (String::new(), 0));
    assert_eq!(take_lines(&mut "a\r\n".into(), true), ("a\r".into(), 1));
}

#[tokio::test]
async fn actual_file_observer_flushes_completion_and_preserves_event_metadata() {
    let fixture = Fixture::new("a\nb\npartial".as_bytes(), true);
    let sink = Sink::default();
    let mut monitor = ShellProcessMonitor::new(Duration::from_secs(60));
    monitor
        .observe(
            &fixture.identity,
            &fixture.execution,
            &sink,
            &fixture.lifetime,
        )
        .await;
    assert!(!monitor.blocks_turn());
    assert_eq!(
        monitor.activity_kind(),
        ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
    );
    assert_eq!(monitor.report.events, 3);
    assert_eq!(monitor.report.exit_code, "3");
    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].text, "Monitor watch (ticks):\na\nb\npartial");
    assert_eq!(events[0].source, MonitorInputSource::Monitor);
    assert_eq!(events[0].delivery, MonitorDelivery::Steer);
    assert_eq!(events[0].activity_name, "watch");
    assert_eq!(events[0].activity_description, "monitor watch event");
    drop(events);
    assert_eq!(
        monitor.complete(&fixture.identity, &sink).unwrap(),
        "Monitor watch (ticks) exited with code 3. Events delivered: 3."
    );
    assert_eq!(
        sink.events.lock().unwrap().len(),
        1,
        "completion is rendered, not sent as a monitor event"
    );
    assert_eq!(sink.completions.lock().unwrap()[0].timeout_ms, "60000");
}

#[tokio::test]
async fn live_file_polling_retains_partial_lines_and_split_utf8() {
    let fixture = Fixture::new(&[0xe7], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_secs(60),
            vec![
                Step::new(200, vec![0x95, 0x8c, b'\n'], false),
                Step::new(200, b"tail".to_vec(), true),
            ],
            &sink,
        )
        .await;
    let text: Vec<_> = sink
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|event| event.text.clone())
        .collect();
    assert_eq!(
        text,
        ["Monitor watch (ticks):\n界", "Monitor watch (ticks):\ntail"]
    );
    assert_eq!(monitor.report.events, 2);
}

#[tokio::test]
async fn initial_bom_is_detected_across_file_appends_but_embedded_bom_is_preserved() {
    let fixture = Fixture::new(&[0xef], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_secs(60),
            vec![
                Step::new(200, vec![0xbb, 0xbf, b'a', b'\n'], false),
                Step::new(200, "\u{feff}b\n".as_bytes().to_vec(), true),
            ],
            &sink,
        )
        .await;
    let events = sink.events.lock().unwrap();
    assert_eq!(events[0].text, "Monitor watch (ticks):\na");
    assert_eq!(events[1].text, "Monitor watch (ticks):\n\u{feff}b");
    assert_eq!(monitor.report.events, 2);
}

#[tokio::test]
async fn incomplete_utf8_is_replaced_only_at_completed_file() {
    let fixture = Fixture::new(&[0xe7], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_secs(60),
            vec![Step::new(200, Vec::new(), true)],
            &sink,
        )
        .await;
    assert_eq!(
        sink.events.lock().unwrap()[0].text,
        "Monitor watch (ticks):\n�"
    );
    assert_eq!(monitor.report.events, 1);
}

#[tokio::test]
async fn flood_trigger_batch_and_later_lines_are_not_delivered() {
    let fixture = Fixture::new(&lines(100), false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_secs(60),
            vec![
                Step::new(200, lines(1), false),
                Step::new(200, lines(5), true),
            ],
            &sink,
        )
        .await;
    assert_eq!(monitor.report.events, 100);
    assert_eq!(monitor.report.reason, MonitorReason::Flooded);
    assert_eq!(sink.events.lock().unwrap().len(), 1);
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(200, 15)]);
}

#[tokio::test]
async fn flood_window_resets_strictly_after_ten_seconds_not_at_boundary() {
    for (milliseconds, flooded) in [(10_000, true), (10_001, false)] {
        let fixture = Fixture::new(&lines(100), false);
        let sink = Sink::default();
        let monitor = fixture
            .run(
                Duration::from_secs(60),
                vec![Step::new(milliseconds, lines(1), true)],
                &sink,
            )
            .await;
        assert_eq!(monitor.report.reason == MonitorReason::Flooded, flooded);
        assert_eq!(monitor.report.events, if flooded { 100 } else { 101 });
        assert_eq!(
            fixture.execution.signals.lock().unwrap().len(),
            usize::from(flooded)
        );
    }
}

#[tokio::test]
async fn timeout_and_kill_grace_use_inclusive_thresholds_and_signal_once() {
    let fixture = Fixture::new(&[], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_millis(1000),
            vec![
                Step::new(999, Vec::new(), false),
                Step::new(1, Vec::new(), false),
                Step::new(4999, Vec::new(), false),
                Step::new(1, Vec::new(), false),
                Step::new(200, Vec::new(), true),
            ],
            &sink,
        )
        .await;
    assert_eq!(
        *fixture.execution.signals.lock().unwrap(),
        [(1000, 15), (6000, 9)]
    );
    assert_eq!(monitor.report.reason, MonitorReason::TimedOut);
    assert_eq!(monitor.report.events, 0);
}

#[tokio::test]
async fn completed_process_returns_before_timeout_and_escalation_checks() {
    let fixture = Fixture::new(&[], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_millis(1000),
            vec![Step::new(1000, Vec::new(), true)],
            &sink,
        )
        .await;
    assert_eq!(monitor.report.reason, MonitorReason::Exited);
    assert!(fixture.execution.signals.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lines_over_flood_threshold_still_deliver_after_timeout_was_already_set() {
    let fixture = Fixture::new(&[], false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::ZERO,
            vec![Step::new(200, lines(101), true)],
            &sink,
        )
        .await;
    assert_eq!(monitor.report.reason, MonitorReason::TimedOut);
    assert_eq!(monitor.report.events, 101);
    assert_eq!(sink.events.lock().unwrap().len(), 1);
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(0, 15)]);
}

#[tokio::test]
async fn flooded_monitor_escalates_after_five_seconds_without_delivering_more() {
    let fixture = Fixture::new(&lines(101), false);
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::from_secs(60),
            vec![
                Step::new(5000, lines(5), false),
                Step::new(200, lines(5), true),
            ],
            &sink,
        )
        .await;
    assert_eq!(monitor.report.events, 0);
    assert_eq!(
        *fixture.execution.signals.lock().unwrap(),
        [(0, 15), (5000, 9)]
    );
    assert!(sink.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_send_does_not_increment_events_and_kills_with_diagnostic_identity() {
    let fixture = Fixture::new(b"a\n", true);
    let sink = Sink {
        send_error: true,
        ..Sink::default()
    };
    let monitor = fixture
        .run(Duration::from_secs(60), Vec::new(), &sink)
        .await;
    assert_eq!(monitor.report.events, 0);
    assert_eq!(monitor.report.reason, MonitorReason::Failed);
    assert_eq!(monitor.report.error, "delivery failed");
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(0, 9)]);
    assert_eq!(
        *sink.failures.lock().unwrap(),
        [(fixture.identity.clone(), "delivery failed".into())]
    );
}

#[tokio::test]
async fn oversized_rendered_utf8_uses_blob_callback_only_above_exact_byte_limit() {
    let prefix = "Monitor watch (ticks):\n".len();
    for extra in [0, 1] {
        let fixture = Fixture::new(&vec![b'a'; MAX_INLINE_BYTES - prefix + extra], true);
        let sink = Sink::default();
        let monitor = fixture
            .run(Duration::from_secs(60), Vec::new(), &sink)
            .await;
        assert_eq!(sink.persisted.lock().unwrap().len(), extra);
        assert_eq!(monitor.report.events, 1);
        if extra == 1 {
            assert_eq!(
                sink.events.lock().unwrap()[0].text,
                "Tool output exceeded 64 KiB and was saved to /fixture/blob."
            );
        }
    }
    let fixture = Fixture::new("界".repeat(22_000).as_bytes(), true);
    let sink = Sink::default();
    fixture
        .run(Duration::from_secs(60), Vec::new(), &sink)
        .await;
    assert_eq!(
        sink.persisted.lock().unwrap().len(),
        1,
        "bound is UTF-8 bytes, not characters"
    );
}

#[tokio::test]
async fn persist_failure_is_failed_observation_without_delivery_or_count() {
    let fixture = Fixture::new(&vec![b'a'; MAX_INLINE_BYTES + 1], true);
    let sink = Sink {
        persist_error: true,
        ..Sink::default()
    };
    let monitor = fixture
        .run(Duration::from_secs(60), Vec::new(), &sink)
        .await;
    assert_eq!(monitor.report.reason, MonitorReason::Failed);
    assert_eq!(monitor.report.events, 0);
    assert!(sink.events.lock().unwrap().is_empty());
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(0, 9)]);
}

#[tokio::test]
async fn lifetime_cancellation_is_not_failed_and_does_not_signal() {
    let fixture = Fixture::new(b"a\n", false);
    let sink = Sink::default();
    let mut step = Step::new(200, Vec::new(), false);
    step.cancel = true;
    let monitor = fixture
        .run(Duration::from_secs(60), vec![step], &sink)
        .await;
    assert_eq!(monitor.report.events, 1);
    assert_eq!(monitor.report.reason, MonitorReason::Exited);
    assert!(monitor.report.error.is_empty());
    assert!(monitor.report.exit_code.is_empty());
    assert!(sink.failures.lock().unwrap().is_empty());
    assert!(fixture.execution.signals.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completion_failure_is_reported_and_killed_after_final_lines() {
    let mut fixture = Fixture::new(b"last", true);
    fixture.execution.completion_error = true;
    let sink = Sink::default();
    let monitor = fixture
        .run(Duration::from_secs(60), Vec::new(), &sink)
        .await;
    assert_eq!(monitor.report.events, 1);
    assert_eq!(monitor.report.reason, MonitorReason::Failed);
    assert_eq!(monitor.report.error, "execution failed");
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(0, 9)]);
}

#[tokio::test]
async fn signal_racing_exit_errors_do_not_change_timeout_reason() {
    let mut fixture = Fixture::new(&[], false);
    fixture.execution.signal_error = true;
    let sink = Sink::default();
    let monitor = fixture
        .run(
            Duration::ZERO,
            vec![Step::new(200, Vec::new(), true)],
            &sink,
        )
        .await;
    assert_eq!(monitor.report.reason, MonitorReason::TimedOut);
    assert!(sink.failures.lock().unwrap().is_empty());
    assert_eq!(monitor.report.exit_code, "3");
}

#[tokio::test]
async fn non_pipe_execution_is_failed_not_silently_accepted() {
    let mut fixture = Fixture::new(&[], true);
    fixture.execution.path = None;
    let sink = Sink::default();
    let monitor = fixture
        .run(Duration::from_secs(60), Vec::new(), &sink)
        .await;
    assert_eq!(monitor.report.reason, MonitorReason::Failed);
    assert_eq!(monitor.report.error, "A monitor requires pipe output.");
    assert_eq!(*fixture.execution.signals.lock().unwrap(), [(0, 9)]);
}

// This is only an adapter: process launch, concurrent drains, completion and
// settlement are the actual T40 implementation, not a monitor-side substitute.
struct RealExecution(crate::process::execution::ShellProcessExecution);
impl MonitorExecution for RealExecution {
    fn stdout_path(&self) -> Option<PathBuf> {
        self.0.stdout_path()
    }
    fn is_completed(&self) -> bool {
        self.0.is_completed()
    }
    fn send_signal(&self, signal: i32) -> io::Result<()> {
        self.0.send_signal(signal)
    }
    fn completion(&self) -> BoxFuture<'_, io::Result<i32>> {
        Box::pin(async move { self.0.wait().await.map(|result| result.exit_code) })
    }
}

fn real_pipe(fixture: &Fixture, command: &str) -> RealExecution {
    let mut spec = crate::process::execution::LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), command.into()];
    RealExecution(
        crate::process::execution::ShellProcessExecution::start_pipe(
            &spec,
            fixture.directory.join("blobs"),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn real_t40_stdout_batches_exclude_stderr_and_report_actual_exit() {
    let fixture = Fixture::new(&[], true);
    let execution = real_pipe(
        &fixture,
        "printf 'a\\nb\\n'; sleep 1; echo c; echo hidden >&2; exit 3",
    );
    let sink = Sink::default();
    let mut monitor = ShellProcessMonitor::new(Duration::from_millis(300_000));
    tokio::time::timeout(
        Duration::from_secs(10),
        monitor.observe(&fixture.identity, &execution, &sink, &fixture.lifetime),
    )
    .await
    .unwrap();
    let result = execution.0.wait().await.unwrap();
    assert_eq!(result.exit_code, 3);
    assert_eq!(result.stdout, "a\nb\nc\n");
    assert_eq!(result.stderr, "hidden\n");
    {
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "Monitor watch (ticks):\na\nb");
        assert_eq!(events[1].text, "Monitor watch (ticks):\nc");
    }
    assert_eq!(monitor.report.events, 3);
    assert_eq!(monitor.report.exit_code, "3");
    assert_eq!(monitor.report.reason, MonitorReason::Exited);
    assert_eq!(monitor.report.timeout_ms, "300000");
    assert_eq!(
        monitor.complete(&fixture.identity, &sink).unwrap(),
        "Monitor watch (ticks) exited with code 3. Events delivered: 3."
    );
    execution.0.settle().await.unwrap();
}

#[tokio::test]
async fn real_t40_timeout_signals_and_joins_the_actual_process() {
    let mut fixture = Fixture::new(&[], true);
    fixture.identity.description = "idle".into();
    let execution = real_pipe(&fixture, "while true; do sleep 0.1; done");
    let sink = Sink::default();
    let mut monitor = ShellProcessMonitor::new(Duration::from_millis(1000));
    tokio::time::timeout(
        Duration::from_secs(10),
        monitor.observe(&fixture.identity, &execution, &sink, &fixture.lifetime),
    )
    .await
    .unwrap();
    assert_eq!(monitor.report.reason, MonitorReason::TimedOut);
    assert_eq!(monitor.report.events, 0);
    assert_eq!(monitor.report.exit_code, "143");
    assert!(sink.events.lock().unwrap().is_empty());
    assert!(execution.0.is_completed());
    assert_eq!(monitor.report.timeout_ms, "1000");
    assert_eq!(
        monitor.complete(&fixture.identity, &sink).unwrap(),
        "Monitor watch (idle) reached its 1000ms timeout and was stopped; start it again if you still need the watch. Events delivered: 0."
    );
    execution.0.settle().await.unwrap();
}

#[tokio::test]
async fn real_t40_flood_batch_is_suppressed_and_actual_process_is_stopped() {
    let mut fixture = Fixture::new(&[], true);
    fixture.identity.description = "flood".into();
    let execution = real_pipe(&fixture, "seq 1 500; while true; do sleep 0.1; done");
    let sink = Sink::default();
    let mut monitor = ShellProcessMonitor::new(Duration::from_millis(300_000));
    tokio::time::timeout(
        Duration::from_secs(10),
        monitor.observe(&fixture.identity, &execution, &sink, &fixture.lifetime),
    )
    .await
    .unwrap();
    assert_eq!(monitor.report.reason, MonitorReason::Flooded);
    assert_eq!(monitor.report.events, 0);
    assert_eq!(monitor.report.exit_code, "143");
    assert!(sink.events.lock().unwrap().is_empty());
    let result = execution.0.wait().await.unwrap();
    assert_eq!(result.stdout.lines().count(), 500);
    assert_eq!(monitor.report.timeout_ms, "300000");
    assert_eq!(
        monitor.complete(&fixture.identity, &sink).unwrap(),
        "Monitor watch (flood) produced too many events and was stopped; start it again with a tighter filter. Events delivered: 0."
    );
    execution.0.settle().await.unwrap();
}

#[tokio::test]
async fn real_t40_external_interrupt_keeps_monitor_reason_exited() {
    let mut fixture = Fixture::new(&[], true);
    fixture.identity.description = "ready".into();
    let execution = real_pipe(&fixture, "echo ready; while true; do sleep 0.1; done");
    let sink = Sink::default();
    let mut monitor = ShellProcessMonitor::new(Duration::from_millis(300_000));
    tokio::time::timeout(Duration::from_secs(10), async {
        let observe = monitor.observe(&fixture.identity, &execution, &sink, &fixture.lifetime);
        let interrupt = async {
            while sink.events.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            execution.0.send_signal(15).unwrap();
        };
        tokio::join!(observe, interrupt);
    })
    .await
    .unwrap();
    assert_eq!(monitor.report.reason, MonitorReason::Exited);
    assert_eq!(monitor.report.events, 1);
    assert_eq!(monitor.report.exit_code, "143");
    assert_eq!(monitor.report.timeout_ms, "300000");
    assert_eq!(
        sink.events.lock().unwrap()[0].text,
        "Monitor watch (ready):\nready"
    );
    assert_eq!(
        monitor.complete(&fixture.identity, &sink).unwrap(),
        "Monitor watch (ready) exited with code 143. Events delivered: 1."
    );
    execution.0.settle().await.unwrap();
}
