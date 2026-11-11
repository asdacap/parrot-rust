use parrot_core::ErrorCode;
use parrot_provider::sessions::Error;
use parrot_provider::sessions::diagnostics::*;
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
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
    no_headers.finish(TraceOutcome::Failed(ErrorCode::Timeout));
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
    assert!(!request.finish(TraceOutcome::Failed(ErrorCode::Io)));
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
    copy.finish(TraceOutcome::Failed(ErrorCode::WebSocketClosed));
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
        (ErrorCode::Cancelled, "cancelled"),
        (ErrorCode::AccessDenied, "access_denied"),
        (ErrorCode::Io, "io"),
        (ErrorCode::Timeout, "timeout"),
        (ErrorCode::Http, "http"),
        (ErrorCode::Argument, "argument"),
        (ErrorCode::InvalidOperation, "invalid_operation"),
        (ErrorCode::WebSocketClosed, "websocket_closed"),
        (ErrorCode::WebSocket, "websocket"),
        (ErrorCode::WireProtocol, "wire_protocol"),
        (ErrorCode::WebSocketUpgrade, "websocket_upgrade"),
        (
            ErrorCode::WebSocketMessageTooLarge,
            "websocket_message_too_large",
        ),
        (ErrorCode::RequestTooLarge, "request_too_large"),
        (ErrorCode::WebSocketTransport, "websocket_transport"),
        (ErrorCode::Unexpected, "unexpected"),
    ] {
        assert_eq!(kind.code(), code);
    }
    assert_eq!(
        parrot_provider::sessions::diagnostic_session::failure_code(&Error::WebSocketTransport {
            message: "wrapper keeps the typed inner classification.".into(),
            cause: Box::new(Error::Io("SECRET".into())),
        }),
        ErrorCode::Io
    );
    assert_eq!(
        ErrorCode::from_io(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SECRET"
        )),
        ErrorCode::AccessDenied
    );
    let clock = MonotonicDiagnosticClock::default();
    let _ = clock.now_millis();
}

#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
enum ArchivedBody {
    Utf8(String),
    Bytes(Vec<u8>),
}

#[derive(serde::Deserialize)]
struct TimestampedResponse {
    received_at: String,
    #[serde(flatten)]
    body: ArchivedBody,
}

#[derive(serde::Deserialize)]
struct TimestampedResponses {
    response: Vec<TimestampedResponse>,
}

#[derive(serde::Deserialize)]
struct ArchivedRequest {
    number: u64,
    transport: String,
    request: Option<ArchivedBody>,
    full_request: Option<ArchivedBody>,
    response: Vec<ArchivedBody>,
    outcome: String,
    error_code: Option<String>,
}

fn archive_call(root: &TestRoot, sink: &Arc<RecordingSink>) -> CallTrace {
    CallTrace::start(
        sink.clone(),
        Arc::new(ManualClock::default()),
        context(),
        Some(Arc::new(LastRequestDumper::new(
            root.0.join("requests/last_requests.json"),
            sink.clone(),
        ))),
    )
}

fn archived_request(root: &TestRoot, number: u64) -> ArchivedRequest {
    serde_json::from_slice(
        &fs::read(root.0.join(format!("requests/request_{number}.json"))).unwrap(),
    )
    .unwrap()
}

#[test]
fn archive_preserves_exact_request_and_response_bytes_before_parsing() {
    let root = TestRoot::new("archive-exact");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let request = call.begin_request("http_sse", "one");
    let initial = archived_request(&root, 1);
    assert_eq!(initial.outcome, "in_progress");
    assert_eq!(initial.request, None);
    assert_eq!(initial.full_request, None);
    let body = b"{\"x\":1e999,\"x\":2}";
    request.dump_prepared_body(body);
    let capture = request.response_capture().unwrap();
    let response = "data: SECRET-RESPONSE-雪\n\n".as_bytes();
    for byte in response {
        capture.append(&[*byte]);
    }
    assert!(request.finish(TraceOutcome::Completed));
    capture.append(b"ignored");
    let record = archived_request(&root, 1);
    assert_eq!(record.number, 1);
    assert_eq!(record.transport, "http_sse");
    assert_eq!(record.full_request, record.request);
    assert_eq!(
        record.request,
        Some(ArchivedBody::Utf8(
            String::from_utf8(body.to_vec()).unwrap()
        ))
    );
    assert_eq!(
        record.response,
        vec![ArchivedBody::Utf8(
            String::from_utf8(response.to_vec()).unwrap()
        )]
    );
    assert_eq!(record.outcome, "completed");
    assert_eq!(record.error_code, None);
    assert_eq!(
        fs::read(root.0.join("requests/last_requests.json")).unwrap(),
        body
    );
    assert!(!format!("{:?}", sink.entries()).contains("SECRET-RESPONSE"));
    assert!(!root.0.join("requests/request_1.json.tmp").exists());
}

#[test]
fn archive_preserves_websocket_boundaries_binary_bytes_and_partial_messages() {
    let root = TestRoot::new("archive-messages");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let request = call.begin_request("websocket", "one");
    request.dump_prepared_body(&[0xff, 0x01]);
    let capture = request.response_capture().unwrap();
    capture.append(b"first");
    capture.finish_message();
    capture.finish_message();
    capture.append(&[0xff]);
    capture.append(&[0x01]);
    assert!(request.finish(TraceOutcome::Failed(ErrorCode::WireProtocol)));
    let record = archived_request(&root, 1);
    assert_eq!(record.full_request, record.request);
    assert_eq!(record.request, Some(ArchivedBody::Bytes(vec![0xff, 0x01])));
    assert_eq!(
        record.response,
        vec![
            ArchivedBody::Utf8("first".into()),
            ArchivedBody::Utf8(String::new()),
            ArchivedBody::Bytes(vec![0xff, 0x01]),
        ]
    );
    assert_eq!(record.outcome, "failed");
    assert_eq!(record.error_code.as_deref(), Some("wire_protocol"));
}

#[test]
fn archive_timestamps_each_response_at_its_last_received_fragment() {
    for outcome in [
        TraceOutcome::Completed,
        TraceOutcome::Failed(ErrorCode::WireProtocol),
        TraceOutcome::Cancelled,
    ] {
        let root = TestRoot::new("archive-timestamps");
        let sink = Arc::new(RecordingSink::default());
        let call = archive_call(&root, &sink);
        let request = call.begin_request("websocket", "one");
        let capture = request.response_capture().unwrap();

        capture.append(b"fir");
        let first_start = SystemTime::now();
        capture.append(b"st");
        let first_end = SystemTime::now();
        capture.finish_message();

        let empty_start = SystemTime::now();
        capture.finish_message();
        let empty_end = SystemTime::now();

        capture.append(&[0xff]);
        let partial_start = SystemTime::now();
        capture.append(&[0x01]);
        let partial_end = SystemTime::now();
        assert!(request.finish(outcome));

        let record: TimestampedResponses =
            serde_json::from_slice(&fs::read(root.0.join("requests/request_1.json")).unwrap())
                .unwrap();
        assert_eq!(record.response.len(), 3);
        let windows = [
            (first_start, first_end),
            (empty_start, empty_end),
            (partial_start, partial_end),
        ];
        for (response, (start, end)) in record.response.iter().zip(windows) {
            assert!(response.received_at.ends_with('Z'));
            let received_at: SystemTime =
                chrono::DateTime::parse_from_rfc3339(&response.received_at)
                    .unwrap()
                    .into();
            assert!(received_at >= start);
            assert!(received_at <= end);
        }
        assert_eq!(
            record
                .response
                .into_iter()
                .map(|response| response.body)
                .collect::<Vec<_>>(),
            vec![
                ArchivedBody::Utf8("first".into()),
                ArchivedBody::Utf8(String::new()),
                ArchivedBody::Bytes(vec![0xff, 0x01]),
            ]
        );
    }
}

#[test]
fn archive_retains_ten_numeric_records_and_continues_after_restart() {
    let root = TestRoot::new("archive-retention");
    let sink = Arc::new(RecordingSink::default());
    fs::create_dir(root.0.join("requests")).unwrap();
    fs::write(root.0.join("requests/unrelated.json"), "keep").unwrap();
    fs::write(root.0.join("last_request.json"), "legacy").unwrap();
    {
        let call = archive_call(&root, &sink);
        for number in 1..=12 {
            let request = call.begin_request("http_sse", &number.to_string());
            request.dump_prepared_body(number.to_string().as_bytes());
            request.finish(TraceOutcome::Completed);
        }
        assert!(!root.0.join("requests/request_1.json").exists());
        assert!(!root.0.join("requests/request_2.json").exists());
        for number in 3..=12 {
            assert_eq!(archived_request(&root, number).number, number);
        }
    }
    let call = archive_call(&root, &sink);
    let request = call.begin_request("http_sse", "after-restart");
    request.dump_prepared_body(b"thirteen");
    request.finish(TraceOutcome::Completed);
    assert_eq!(archived_request(&root, 13).number, 13);
    assert!(!root.0.join("requests/request_3.json").exists());
    assert_eq!(fs::read_dir(root.0.join("requests")).unwrap().count(), 12);
    assert_eq!(
        fs::read(root.0.join("requests/last_requests.json")).unwrap(),
        b"thirteen"
    );
    assert_eq!(
        fs::read(root.0.join("requests/unrelated.json")).unwrap(),
        b"keep"
    );
    assert_eq!(
        fs::read(root.0.join("last_request.json")).unwrap(),
        b"legacy"
    );
}

#[test]
fn archive_eviction_prevents_late_completion_or_preparation_from_recreating_records() {
    let root = TestRoot::new("archive-late");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let oldest = call.begin_request("http_sse", "oldest");
    oldest.dump_prepared_body(b"oldest");
    let capture = oldest.response_capture().unwrap();
    capture.append(b"discard this buffer");
    let mut pending = Vec::new();
    for number in 2..=12 {
        let request = call.begin_request("http_sse", &number.to_string());
        request.dump_prepared_body(number.to_string().as_bytes());
        pending.push(request);
    }
    for request in pending.into_iter().rev() {
        request.finish(TraceOutcome::Completed);
    }
    capture.append(b"not buffered");
    capture.finish_message();
    oldest.dump_prepared_body(b"late-old-body");
    oldest.finish(TraceOutcome::Completed);
    assert!(!root.0.join("requests/request_1.json").exists());
    assert!(!root.0.join("requests/request_2.json").exists());
    assert_eq!(fs::read_dir(root.0.join("requests")).unwrap().count(), 11);
    assert_eq!(
        fs::read(root.0.join("requests/last_requests.json")).unwrap(),
        b"12"
    );
    assert_eq!(archived_request(&root, 12).outcome, "completed");
}

#[test]
fn archive_cancellation_snapshots_partial_response_without_another_poll() {
    let root = TestRoot::new("archive-cancel");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let request = call.begin_request("http_sse", "partial");
    request.dump_prepared_body(b"request");
    let capture = request.response_capture().unwrap();
    capture.append(b"data: partial");
    call.cancel();
    capture.append(b"too late");
    assert!(!request.finish(TraceOutcome::Completed));
    let record = archived_request(&root, 1);
    assert_eq!(
        record.response,
        vec![ArchivedBody::Utf8("data: partial".into())]
    );
    assert_eq!(record.outcome, "cancelled");
    assert_eq!(record.error_code.as_deref(), Some("cancelled"));
    let entries = sink.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|event| event.operation == "request_finished")
            .count(),
        1
    );
    assert_eq!(entries.last().unwrap().operation, "call_finished");
}

#[test]
fn archive_records_null_requests_and_disposal_without_replacing_latest_body() {
    let root = TestRoot::new("archive-null");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let first = call.begin_request("http_sse", "first");
    first.dump_prepared_body(b"latest");
    first.finish(TraceOutcome::Completed);
    let failed = call.begin_request("websocket", "connect-failure");
    failed.finish(TraceOutcome::Failed(ErrorCode::WebSocketUpgrade));
    drop(call.begin_request("websocket", "disposed"));
    let record = archived_request(&root, 2);
    assert_eq!(record.request, None);
    assert_eq!(record.full_request, None);
    assert!(record.response.is_empty());
    assert_eq!(record.outcome, "failed");
    assert_eq!(record.error_code.as_deref(), Some("websocket_upgrade"));
    assert_eq!(archived_request(&root, 3).outcome, "disposed");
    assert_eq!(
        fs::read(root.0.join("requests/last_requests.json")).unwrap(),
        b"latest"
    );
}

#[test]
fn archive_isolates_agent_counters_and_capture_disabled_paths() {
    let first = TestRoot::new("archive-first-agent");
    let second = TestRoot::new("archive-second-agent");
    let sink = Arc::new(RecordingSink::default());
    for root in [&first, &second] {
        let call = archive_call(root, &sink);
        call.begin_request("http_sse", "one")
            .finish(TraceOutcome::Completed);
        assert_eq!(archived_request(root, 1).number, 1);
        assert!(!root.0.join("requests/request_2.json").exists());
    }
    let disabled = call(&sink, &Arc::new(ManualClock::default()));
    assert!(
        disabled
            .begin_request("http_sse", "one")
            .response_capture()
            .is_none()
    );
}

#[test]
fn archive_serializes_concurrent_attempts_and_retention() {
    let root = TestRoot::new("archive-concurrent");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    thread::scope(|scope| {
        for number in 0..32 {
            let call = call.clone();
            scope.spawn(move || {
                let request = call.begin_request("http_sse", &number.to_string());
                request.dump_prepared_body(number.to_string().as_bytes());
                request.response_capture().unwrap().append(b"response");
                request.finish(TraceOutcome::Completed);
            });
        }
    });
    for number in 23..=32 {
        let record = archived_request(&root, number);
        assert_eq!(record.number, number);
        assert_eq!(record.outcome, "completed");
    }
    assert_eq!(fs::read_dir(root.0.join("requests")).unwrap().count(), 11);
    assert!(
        sink.entries()
            .iter()
            .all(|event| event.operation != "last_request_dump_failed")
    );
}

#[test]
fn archive_io_failures_do_not_change_trace_outcomes_or_expose_payloads() {
    let root = TestRoot::new("archive-io-failure");
    fs::write(root.0.join("requests"), "not a directory").unwrap();
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let request = call.begin_request("http_sse", "one");
    request.dump_prepared_body(b"SECRET-REQUEST");
    assert!(request.response_capture().is_none());
    assert!(request.finish(TraceOutcome::Completed));
    let events = sink.entries();
    let failures: Vec<_> = events
        .iter()
        .filter(|event| event.operation == "last_request_dump_failed")
        .collect();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].error_code.is_some());
    assert_eq!(events.last().unwrap().outcome.as_deref(), Some("completed"));
    assert!(!format!("{events:?}").contains("SECRET"));
    assert!(!format!("{events:?}").contains(root.0.to_str().unwrap()));
}

#[test]
fn archive_failed_terminal_rename_cleans_temporary_file_and_remains_nonthrowing() {
    let root = TestRoot::new("archive-terminal-rename");
    let sink = Arc::new(RecordingSink::default());
    let call = archive_call(&root, &sink);
    let request = call.begin_request("http_sse", "one");
    let path = root.0.join("requests/request_1.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    request
        .response_capture()
        .unwrap()
        .append(b"SECRET-RESPONSE");
    assert!(request.finish(TraceOutcome::Completed));
    assert!(path.is_dir());
    assert!(!root.0.join("requests/request_1.json.tmp").exists());
    let events = sink.entries();
    assert!(
        events
            .iter()
            .any(|event| event.operation == "last_request_dump_failed")
    );
    assert_eq!(events.last().unwrap().outcome.as_deref(), Some("completed"));
    assert!(!format!("{events:?}").contains("SECRET"));
}
