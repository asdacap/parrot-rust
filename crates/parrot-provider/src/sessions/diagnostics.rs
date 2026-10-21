//! Metadata-only provider diagnostics and atomic last-request capture.
//!
//! The caller supplies correlation/request IDs and invokes `cancel` from its
//! cancellation watcher, so terminal records do not depend on another read or
//! on the consumer disposing its stream.
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    Information,
    Warning,
    Error,
}

/// Allowlisted operational metadata. No request body, headers, exception text,
/// image content or filesystem path is included in a diagnostic record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEvent {
    pub category: String,
    pub operation: String,
    pub severity: DiagnosticSeverity,
    pub user_session_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub correlation_id: Option<String>,
    pub request_id: Option<String>,
    pub transport: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<String>,
    pub duration_milliseconds: Option<u64>,
    pub count: Option<i64>,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub close_status: Option<i32>,
    pub error_code: Option<String>,
}
impl DiagnosticEvent {
    pub fn provider(operation: impl Into<String>, severity: DiagnosticSeverity) -> Self {
        Self {
            category: "provider".into(),
            operation: operation.into(),
            severity,
            user_session_id: None,
            agent_session_id: None,
            correlation_id: None,
            request_id: None,
            transport: None,
            provider_id: None,
            model_id: None,
            tool_name: None,
            outcome: None,
            duration_milliseconds: None,
            count: None,
            request_bytes: None,
            response_bytes: None,
            close_status: None,
            error_code: None,
        }
    }
}

pub trait DiagnosticSink: Send + Sync {
    fn write(&self, event: DiagnosticEvent);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Cancelled,
    AccessDenied,
    Io,
    Timeout,
    Http,
    Argument,
    InvalidOperation,
    WebSocketClosed,
    WebSocket,
    WireProtocol,
    WebSocketUpgrade,
    WebSocketMessageTooLarge,
    RequestTooLarge,
    WebSocketTransport,
    Unexpected,
}
impl FailureKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AccessDenied => "access_denied",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Http => "http",
            Self::Argument => "argument",
            Self::InvalidOperation => "invalid_operation",
            Self::WebSocketClosed => "websocket_closed",
            Self::WebSocket => "websocket",
            Self::WireProtocol => "wire_protocol",
            Self::WebSocketUpgrade => "websocket_upgrade",
            Self::WebSocketMessageTooLarge => "websocket_message_too_large",
            Self::RequestTooLarge => "request_too_large",
            Self::WebSocketTransport => "websocket_transport",
            Self::Unexpected => "unexpected",
        }
    }

    /// Source transport wrappers expose the inner failure classification.
    pub fn websocket_transport(inner: Option<Self>) -> Self {
        inner.unwrap_or(Self::WebSocketTransport)
    }

    pub fn from_io(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::InvalidInput => Self::Argument,
            _ => Self::Io,
        }
    }
}

/// Replaces `path` with the complete actual prepared body. Failures are reported
/// only by category; payloads and error strings never enter diagnostics.
pub struct LastRequestDumper {
    path: PathBuf,
    sink: Arc<dyn DiagnosticSink>,
    gate: Mutex<()>,
}
impl LastRequestDumper {
    pub fn new(path: impl Into<PathBuf>, sink: Arc<dyn DiagnosticSink>) -> Self {
        Self {
            path: path.into(),
            sink,
            gate: Mutex::new(()),
        }
    }

    pub fn dump(&self, body: &[u8]) {
        let failure = {
            let _guard = lock(&self.gate);
            let temp = temp_path(&self.path);
            let result = fs::write(&temp, body).and_then(|()| fs::rename(&temp, &self.path));
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result.err()
        };
        if let Some(error) = failure {
            let mut event =
                DiagnosticEvent::provider("last_request_dump_failed", DiagnosticSeverity::Error);
            event.error_code = Some(FailureKind::from_io(&error).code().into());
            self.sink.write(event);
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    PathBuf::from(temp)
}

pub trait DiagnosticClock: Send + Sync {
    fn now_millis(&self) -> u64;
}

pub struct MonotonicDiagnosticClock {
    origin: Instant,
}
impl Default for MonotonicDiagnosticClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}
impl DiagnosticClock for MonotonicDiagnosticClock {
    fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallContext {
    pub agent_session_id: String,
    pub correlation_id: String,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceOutcome {
    Completed,
    Disposed,
    Cancelled,
    Failed(FailureKind),
}
impl TraceOutcome {
    fn fields(self) -> (&'static str, DiagnosticSeverity, Option<FailureKind>) {
        match self {
            Self::Completed => ("completed", DiagnosticSeverity::Information, None),
            Self::Disposed => ("disposed", DiagnosticSeverity::Information, None),
            Self::Cancelled | Self::Failed(FailureKind::Cancelled) => (
                "cancelled",
                DiagnosticSeverity::Information,
                Some(FailureKind::Cancelled),
            ),
            Self::Failed(kind) => ("failed", DiagnosticSeverity::Error, Some(kind)),
        }
    }
}

#[derive(Default)]
struct AttemptState {
    recorded: bool,
    request_bytes: Option<u64>,
    response_bytes: Option<u64>,
    response_bytes_snapshot: Option<Arc<dyn Fn() -> Option<u64> + Send + Sync>>,
    close_status: Option<i32>,
}

struct TraceState {
    sink: Arc<dyn DiagnosticSink>,
    clock: Arc<dyn DiagnosticClock>,
    entry: DiagnosticEvent,
    started: u64,
    terminal_operation: &'static str,
    state: Mutex<AttemptState>,
}
impl TraceState {
    fn start(
        sink: Arc<dyn DiagnosticSink>,
        clock: Arc<dyn DiagnosticClock>,
        entry: DiagnosticEvent,
        terminal_operation: &'static str,
    ) -> Arc<Self> {
        let started = clock.now_millis();
        sink.write(entry.clone());
        Arc::new(Self {
            sink,
            clock,
            entry,
            started,
            terminal_operation,
            state: Mutex::new(AttemptState::default()),
        })
    }

    fn finish(&self, outcome: TraceOutcome) -> bool {
        let mut event = self.entry.clone();
        {
            let mut state = lock(&self.state);
            if state.recorded {
                return false;
            }
            state.recorded = true;
            if self.terminal_operation == "request_finished" {
                event.request_bytes = state.request_bytes;
                event.response_bytes = state
                    .response_bytes_snapshot
                    .as_ref()
                    .map_or(state.response_bytes, |snapshot| snapshot());
                event.close_status = state.close_status;
            }
        }
        let (outcome, severity, failure) = outcome.fields();
        event.operation = self.terminal_operation.into();
        event.outcome = Some(outcome.into());
        event.severity = severity;
        event.error_code = failure.map(|kind| kind.code().into());
        event.duration_milliseconds = Some(self.clock.now_millis().saturating_sub(self.started));
        self.sink.write(event);
        true
    }

    fn event(&self, operation: &str) -> DiagnosticEvent {
        let mut event = self.entry.clone();
        event.operation = operation.into();
        event
    }
}
impl Drop for TraceState {
    fn drop(&mut self) {
        self.finish(TraceOutcome::Disposed);
    }
}

#[derive(Default)]
struct RequestRegistrations {
    cancelled: bool,
    requests: Vec<Weak<TraceState>>,
}

/// One logical provider call, including any retry attempts.
#[derive(Clone)]
pub struct CallTrace {
    trace: Arc<TraceState>,
    dumper: Option<Arc<LastRequestDumper>>,
    requests: Arc<Mutex<RequestRegistrations>>,
}
impl CallTrace {
    pub fn start(
        sink: Arc<dyn DiagnosticSink>,
        clock: Arc<dyn DiagnosticClock>,
        context: CallContext,
        dumper: Option<Arc<LastRequestDumper>>,
    ) -> Self {
        let mut entry = DiagnosticEvent::provider("call_started", DiagnosticSeverity::Information);
        entry.agent_session_id = Some(context.agent_session_id);
        entry.correlation_id = Some(context.correlation_id);
        entry.provider_id = Some(context.provider_id);
        entry.model_id = Some(context.model_id);
        Self {
            trace: TraceState::start(sink, clock, entry, "call_finished"),
            dumper,
            requests: Arc::new(Mutex::new(RequestRegistrations::default())),
        }
    }

    pub fn begin_request(&self, transport: &str, request_id: &str) -> RequestTrace {
        let mut entry = self.trace.event("request_started");
        entry.transport = Some(transport.into());
        entry.request_id = Some(request_id.into());
        let request = RequestTrace {
            trace: TraceState::start(
                self.trace.sink.clone(),
                self.trace.clock.clone(),
                entry,
                "request_finished",
            ),
        };
        let cancelled = {
            let mut registrations = lock(&self.requests);
            registrations
                .requests
                .retain(|request| request.strong_count() > 0);
            registrations.requests.push(Arc::downgrade(&request.trace));
            registrations.cancelled
        };
        if cancelled {
            request.cancel();
        }
        request
    }

    /// Feed the adapter's real prepared bytes, never reconstructed diagnostic JSON.
    pub fn dump_prepared_body(&self, body: &[u8]) {
        if let Some(dumper) = &self.dumper {
            dumper.dump(body);
        }
    }

    pub fn record_retry(&self, attempt: i32, delay: Duration) {
        let mut event = self.trace.event("call_retry");
        event.severity = DiagnosticSeverity::Warning;
        event.count = Some(i64::from(attempt));
        event.duration_milliseconds = Some(delay.as_millis() as u64);
        self.trace.sink.write(event);
    }

    pub fn record_tokens(&self, input: i64, cached_input: i64, output: i64) {
        for (operation, count) in [
            ("input_tokens", input),
            ("cached_input_tokens", cached_input),
            ("output_tokens", output),
        ] {
            let mut event = self.trace.event(operation);
            event.count = Some(count);
            self.trace.sink.write(event);
        }
    }

    pub fn finish(&self, outcome: TraceOutcome) -> bool {
        self.trace.finish(outcome)
    }

    pub fn cancel(&self) -> bool {
        let requests: Vec<_> = {
            let mut registrations = lock(&self.requests);
            registrations.cancelled = true;
            registrations
                .requests
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .collect()
        };
        for request in requests {
            request.finish(TraceOutcome::Cancelled);
        }
        self.finish(TraceOutcome::Cancelled)
    }

    pub fn observe_cancellation(&self, is_cancelled: bool) {
        if is_cancelled {
            self.cancel();
        }
    }
}

/// One transport attempt. Header receipt initializes response bytes to zero;
/// subsequent byte counts and close status are included only in its terminal.
#[derive(Clone)]
pub struct RequestTrace {
    trace: Arc<TraceState>,
}
impl RequestTrace {
    /// Reads the owning transport's live byte counter when a terminal is recorded,
    /// including cancellation while an SSE event is only partially received.
    pub fn track_response_bytes(&self, snapshot: Arc<dyn Fn() -> Option<u64> + Send + Sync>) {
        lock(&self.trace.state).response_bytes_snapshot = Some(snapshot);
    }
    pub fn record_request_bytes(&self, count: u64) {
        lock(&self.trace.state).request_bytes = Some(count);
        let mut event = self.trace.event("request_size");
        event.request_bytes = Some(count);
        self.trace.sink.write(event);
    }

    pub fn mark_response_obtained(&self) {
        lock(&self.trace.state).response_bytes.get_or_insert(0);
    }

    pub fn record_response_bytes(&self, count: u64) {
        *lock(&self.trace.state).response_bytes.get_or_insert(0) += count;
    }

    pub fn record_close_status(&self, status: i32) {
        lock(&self.trace.state).close_status = Some(status);
    }

    pub fn finish(&self, outcome: TraceOutcome) -> bool {
        self.trace.finish(outcome)
    }

    pub fn cancel(&self) -> bool {
        self.finish(TraceOutcome::Cancelled)
    }

    pub fn observe_cancellation(&self, is_cancelled: bool) {
        if is_cancelled {
            self.cancel();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
