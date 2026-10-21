use parrot_provider::sessions::diagnostics::*;
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for RecordingSink {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
impl RecordingSink {
    fn entries(&self) -> Vec<DiagnosticEvent> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct ManualClock(AtomicU64);
impl DiagnosticClock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl ManualClock {
    fn advance(&self, millis: u64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

fn context() -> CallContext {
    CallContext {
        agent_session_id: "agent".into(),
        correlation_id: "call-1".into(),
        provider_id: "provider".into(),
        model_id: "model".into(),
    }
}
fn call(sink: &Arc<RecordingSink>, clock: &Arc<ManualClock>) -> CallTrace {
    CallTrace::start(sink.clone(), clock.clone(), context(), None)
}

struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!(
                "t30-diagnostics-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self) -> PathBuf {
        self.0.join("last_request.json")
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn dump_replaces_latest_body_without_structure_or_numeric_rewriting() {
    for (first, second) in [
        ("first-body", "second-body"),
        ("{}", "{}"),
        ("{\"a\":1}", "{\"raw\":1e999,\"x\":0,\"x\":7}"),
    ] {
        let root = TestRoot::new("replace");
        let sink = Arc::new(RecordingSink::default());
        let dumper = LastRequestDumper::new(root.file(), sink.clone());
        dumper.dump(first.as_bytes());
        assert_eq!(fs::read(root.file()).unwrap(), first.as_bytes());
        dumper.dump(second.as_bytes());
        assert_eq!(fs::read(root.file()).unwrap(), second.as_bytes());
        assert!(!root.file().with_file_name("last_request.json.tmp").exists());
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
        assert!(sink.entries().is_empty());
    }
}

#[test]
fn dump_is_safe_under_concurrent_calls() {
    let root = TestRoot::new("concurrent");
    let sink = Arc::new(RecordingSink::default());
    let dumper = Arc::new(LastRequestDumper::new(root.file(), sink.clone()));
    let bodies: Vec<Vec<u8>> = (0..32)
        .map(|index| format!("request-{index}{}", "x".repeat(1024)).into_bytes())
        .collect();
    thread::scope(|scope| {
        for index in 0..32 {
            let body = &bodies[index % 7];
            let dumper = dumper.clone();
            scope.spawn(move || dumper.dump(body));
        }
    });
    let winner = fs::read(root.file()).unwrap();
    assert!(bodies.contains(&winner));
    assert!(!root.file().with_file_name("last_request.json.tmp").exists());
    assert!(sink.entries().is_empty());
}

#[test]
fn dump_failure_is_nonthrowing_and_contains_no_path_or_payload() {
    let root = TestRoot::new("blocked");
    let blocked = root.0.join("SECRET-PATH-CANARY");
    fs::write(&blocked, "not-a-directory").unwrap();
    let sink = Arc::new(RecordingSink::default());
    let dumper = LastRequestDumper::new(blocked.join("last_request.json"), sink.clone());
    dumper.dump(b"SECRET-BODY-CANARY");
    assert!(blocked.is_file());
    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].category, "provider");
    assert_eq!(entries[0].operation, "last_request_dump_failed");
    assert_eq!(entries[0].severity, DiagnosticSeverity::Error);
    assert!(entries[0].error_code.is_some());
    let diagnostic = format!("{entries:?}");
    assert!(!diagnostic.contains("SECRET"));
    assert!(!diagnostic.contains("last_request.json"));
}

#[test]
fn dump_reports_source_failure_payload_without_throwing() {
    let root = TestRoot::new("source-blocked");
    let blocked = root.0.join("blocked");
    fs::write(&blocked, "not-a-directory").unwrap();
    let sink = Arc::new(RecordingSink::default());
    let dumper = LastRequestDumper::new(blocked.join("last_request.json"), sink.clone());
    dumper.dump(&[1, 2, 3]);
    assert!(blocked.is_file());
    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].category, "provider");
    assert_eq!(entries[0].operation, "last_request_dump_failed");
    assert_eq!(entries[0].severity, DiagnosticSeverity::Error);
    assert!(entries[0].error_code.is_some());
}

#[test]
fn failed_rename_removes_temporary_file_and_keeps_existing_destination() {
    let root = TestRoot::new("rename-failure");
    let destination = root.file();
    fs::create_dir(&destination).unwrap();
    let sink = Arc::new(RecordingSink::default());
    let dumper = LastRequestDumper::new(&destination, sink.clone());
    dumper.dump(b"new body");
    assert!(destination.is_dir());
    assert!(!destination.with_file_name("last_request.json.tmp").exists());
    assert_eq!(sink.entries().len(), 1);
}

#[test]
fn cancellation_snapshots_live_response_bytes_before_the_pending_event_returns() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let trace = call(&sink, &clock);
    let request = trace.begin_request("http_sse", "partial");
    let bytes = Arc::new(AtomicU64::new(0));
    let obtained = Arc::new(AtomicBool::new(false));
    request.track_response_bytes(Arc::new({
        let bytes = bytes.clone();
        let obtained = obtained.clone();
        move || {
            obtained
                .load(Ordering::Acquire)
                .then(|| bytes.load(Ordering::Acquire))
        }
    }));
    obtained.store(true, Ordering::Release);
    bytes.store(6, Ordering::Release);
    trace.cancel();
    bytes.store(9, Ordering::Release);
    assert!(!request.finish(TraceOutcome::Completed));
    let entries = sink.entries();
    let terminals: Vec<_> = entries
        .iter()
        .filter(|event| event.operation == "request_finished")
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].response_bytes, Some(6));
    assert_eq!(terminals[0].outcome.as_deref(), Some("cancelled"));
    assert_eq!(entries[entries.len() - 1].operation, "call_finished");
}

#[test]
fn prepared_body_hook_dumps_actual_bytes_without_adding_payload_to_diagnostics() {
    let root = TestRoot::new("hook");
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let dumper = Arc::new(LastRequestDumper::new(root.file(), sink.clone()));
    let trace = CallTrace::start(sink.clone(), clock, context(), Some(dumper));
    let body = b"{\"messages\":[{\"content\":\"SECRET-PROMPT-CANARY\"}],\"schema\":1e999}";
    trace.dump_prepared_body(body);
    assert_eq!(fs::read(root.file()).unwrap(), body);
    trace.finish(TraceOutcome::Completed);
    assert!(!format!("{:?}", sink.entries()).contains("SECRET-PROMPT-CANARY"));
}

#[test]
fn request_hooks_preserve_phase_counter_semantics_and_terminal_metadata() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let call = call(&sink, &clock);
    let request = call.begin_request("http_sse", "request-1");
    request.record_request_bytes(123);
    request.mark_response_obtained();
    request.record_response_bytes(17);
    request.mark_response_obtained();
    request.record_response_bytes(5);
    request.record_close_status(1000);
    clock.advance(9);
    assert!(request.finish(TraceOutcome::Completed));
    assert!(!request.finish(TraceOutcome::Disposed));
    call.record_retry(2, Duration::from_millis(400));
    call.record_tokens(11, 5, 3);
    clock.advance(4);
    call.finish(TraceOutcome::Completed);
    let entries = sink.entries();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>(),
        [
            "call_started",
            "request_started",
            "request_size",
            "request_finished",
            "call_retry",
            "input_tokens",
            "cached_input_tokens",
            "output_tokens",
            "call_finished"
        ]
    );
    let size = &entries[2];
    assert_eq!(size.request_bytes, Some(123));
    assert_eq!(size.response_bytes, None);
    let finished = &entries[3];
    assert_eq!(finished.request_bytes, Some(123));
    assert_eq!(finished.response_bytes, Some(22));
    assert_eq!(finished.close_status, Some(1000));
    assert_eq!(finished.duration_milliseconds, Some(9));
    assert_eq!(finished.request_id.as_deref(), Some("request-1"));
    assert_eq!(finished.transport.as_deref(), Some("http_sse"));
    assert_eq!(finished.correlation_id.as_deref(), Some("call-1"));
    assert_eq!(entries[4].severity, DiagnosticSeverity::Warning);
    assert_eq!(entries[4].count, Some(2));
    assert_eq!(entries[4].duration_milliseconds, Some(400));
    assert_eq!(
        entries[5..8]
            .iter()
            .map(|entry| entry.count)
            .collect::<Vec<_>>(),
        [Some(11), Some(5), Some(3)]
    );
    let call_finished = entries.last().unwrap();
    assert_eq!(call_finished.duration_milliseconds, Some(13));
    assert_eq!(call_finished.request_bytes, None);
    assert_eq!(call_finished.response_bytes, None);
}

#[test]
fn response_headers_without_a_body_read_report_zero_and_no_headers_report_none() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let call = call(&sink, &clock);
    let request = call.begin_request("http_sse", "headers");
    request.mark_response_obtained();
    request.finish(TraceOutcome::Disposed);
    let no_headers = call.begin_request("http_sse", "no-headers");
    no_headers.finish(TraceOutcome::Failed(FailureKind::Timeout));
    let finished: Vec<_> = sink
        .entries()
        .into_iter()
        .filter(|event| event.operation == "request_finished")
        .collect();
    assert_eq!(finished[0].response_bytes, Some(0));
    assert_eq!(finished[1].response_bytes, None);
    assert_eq!(finished[1].outcome.as_deref(), Some("failed"));
    assert_eq!(finished[1].error_code.as_deref(), Some("timeout"));
}

#[test]
fn cancellation_records_immediately_without_another_read_or_dispose_and_only_once() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let call = call(&sink, &clock);
    let request = call.begin_request("websocket", "request-1");
    request.record_request_bytes(3);
    request.record_request_bytes(7);
    clock.advance(5);
    request.observe_cancellation(true);
    call.observe_cancellation(true);
    let entries = sink.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|event| event.operation == "request_finished")
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|event| event.operation == "call_finished")
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .find(|event| event.operation == "request_finished")
            .unwrap()
            .request_bytes,
        Some(7)
    );
    assert_eq!(
        entries.last().unwrap().outcome.as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        entries.last().unwrap().severity,
        DiagnosticSeverity::Information
    );
    assert_eq!(
        entries.last().unwrap().error_code.as_deref(),
        Some("cancelled")
    );
    assert!(!request.finish(TraceOutcome::Failed(FailureKind::Io)));
    assert!(!call.finish(TraceOutcome::Completed));
    drop(request);
    drop(call);
    assert_eq!(sink.entries(), entries);
}

#[test]
fn terminal_drop_and_failure_are_once_only_even_with_cloned_handles() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let call = call(&sink, &clock);
    let request = call.begin_request("websocket", "request-1");
    let copy = request.clone();
    drop(request);
    assert_eq!(sink.entries().len(), 2);
    copy.finish(TraceOutcome::Failed(FailureKind::WebSocketClosed));
    drop(copy);
    drop(call);
    let finished: Vec<_> = sink
        .entries()
        .into_iter()
        .filter(|event| event.operation.ends_with("finished"))
        .collect();
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0].outcome.as_deref(), Some("failed"));
    assert_eq!(finished[0].error_code.as_deref(), Some("websocket_closed"));
    assert_eq!(finished[0].severity, DiagnosticSeverity::Error);
    assert_eq!(finished[1].outcome.as_deref(), Some("disposed"));
    assert_eq!(finished[1].error_code, None);
}

#[test]
fn concurrent_terminal_signals_write_one_terminal_per_scope() {
    let sink = Arc::new(RecordingSink::default());
    let clock = Arc::new(ManualClock::default());
    let call = call(&sink, &clock);
    thread::scope(|scope| {
        for _ in 0..16 {
            let call = call.clone();
            scope.spawn(move || {
                call.cancel();
            });
        }
    });
    assert_eq!(
        sink.entries()
            .iter()
            .filter(|event| event.operation == "call_finished")
            .count(),
        1
    );
}

#[test]
fn failure_classification_is_allowlisted_and_transport_wrappers_use_inner_kind() {
    for (kind, code) in [
        (FailureKind::Cancelled, "cancelled"),
        (FailureKind::AccessDenied, "access_denied"),
        (FailureKind::Io, "io"),
        (FailureKind::Timeout, "timeout"),
        (FailureKind::Http, "http"),
        (FailureKind::Argument, "argument"),
        (FailureKind::InvalidOperation, "invalid_operation"),
        (FailureKind::WebSocketClosed, "websocket_closed"),
        (FailureKind::WebSocket, "websocket"),
        (FailureKind::WireProtocol, "wire_protocol"),
        (FailureKind::WebSocketUpgrade, "websocket_upgrade"),
        (
            FailureKind::WebSocketMessageTooLarge,
            "websocket_message_too_large",
        ),
        (FailureKind::RequestTooLarge, "request_too_large"),
        (FailureKind::WebSocketTransport, "websocket_transport"),
        (FailureKind::Unexpected, "unexpected"),
    ] {
        assert_eq!(kind.code(), code);
    }
    assert_eq!(
        FailureKind::websocket_transport(Some(FailureKind::Io)),
        FailureKind::Io
    );
    assert_eq!(
        FailureKind::websocket_transport(None),
        FailureKind::WebSocketTransport
    );
    assert_eq!(
        FailureKind::from_io(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SECRET"
        )),
        FailureKind::AccessDenied
    );
    let clock = MonotonicDiagnosticClock::default();
    let _ = clock.now_millis();
}
