use parrot_storage::diagnostics::{
    DiagnosticEvent, DiagnosticFailure, DiagnosticScope, DiagnosticSeverity, FailureKind,
    FileDiagnosticLog, InvalidDiagnosticInstanceId,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-storage-diagnostics-{label}-{}-{}",
            std::process::id(),
            FileDiagnosticLog::create_instance_id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn warning_collector(
    warnings: Arc<std::sync::Mutex<Vec<String>>>,
) -> impl Fn(&str) + Send + Sync + 'static {
    move |message| warnings.lock().unwrap().push(message.to_owned())
}

fn warning_counter(
    counter: Arc<std::sync::atomic::AtomicUsize>,
) -> impl Fn(&str) + Send + Sync + 'static {
    move |message| {
        assert_eq!(
            message,
            "parrot: operational logging disabled for one log scope because its log could not be written."
        );
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn event(operation: &str) -> DiagnosticEvent {
    DiagnosticEvent::new("session", operation, DiagnosticSeverity::Information)
}

fn failure(kind: FailureKind) -> DiagnosticFailure<'static> {
    DiagnosticFailure {
        kind,
        message: "sk-parrot-fake-token-canary",
        path: Some("/private/credential-path-canary"),
    }
}

#[test]
fn session_records_are_bounded_escaped_correlated_and_appended() {
    let directory = TempDirectory::new("session");
    let path = directory
        .path()
        .join("selected-session")
        .join("diagnostics.log");
    let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = FileDiagnosticLog::open_session_with_clock(
        &path,
        "instance-one",
        "session-test",
        warning_collector(Arc::clone(&warnings)),
        || UNIX_EPOCH,
    )
    .unwrap();
    let mut first = DiagnosticEvent::new("tools", "finished", DiagnosticSeverity::Error);
    first.agent_session_id = Some("agent-one".into());
    first.user_session_id = Some("wrong-session".into());
    first.correlation_id = Some("call-one".into());
    first.request_id = Some("request-one".into());
    first.transport = Some("https".into());
    first.provider_id = Some("provider-one".into());
    first.model_id = Some("model-one".into());
    first.outcome = Some(format!("{}😀{}", "x".repeat(255), "x".repeat(10_000)));
    first.count = Some(-2);
    first.request_bytes = Some(0);
    first.response_bytes = Some(4_294_967_296);
    first.close_status = Some(1000);
    first.tool_name = Some("read\n\"\\\u{1b}\u{200b}\u{2028}\u{e0001}".into());
    first.duration_milliseconds = Some(123);
    first.error_code = Some(DiagnosticEvent::classify_failure(&failure(FailureKind::Io)).into());
    log.write(&first);
    assert_eq!(log.scope(), DiagnosticScope::Session);
    let visible_before_drop = fs::read_to_string(&path).unwrap();
    assert!(visible_before_drop.contains("event=\"finished\""));
    assert!(!visible_before_drop.contains("sk-parrot-fake-token-canary"));
    assert!(!visible_before_drop.contains("/private/credential-path-canary"));
    drop(log);

    let resumed = FileDiagnosticLog::open_session_with_clock(
        &path,
        "instance-two",
        "session-test",
        warning_collector(Arc::clone(&warnings)),
        || UNIX_EPOCH,
    )
    .unwrap();
    resumed.write(&event("resumed"));

    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(warnings.lock().unwrap().is_empty());
    assert!(lines[0].starts_with("1970-01-01T00:00:00.0000000+00:00 ERROR"));
    assert!(
        lines[0].contains("session=\"session-test\" agent=\"agent-one\" correlation=\"call-one\"")
    );
    assert!(lines[0].contains("tool=\"read\\u000a\\\"\\\\\\u001b\\u200b\\u2028\u{e0001}\""));
    assert!(lines[0].contains("[truncated]"));
    assert!(lines[0].contains(
        "request=\"request-one\" transport=\"https\" provider=\"provider-one\" model=\"model-one\""
    ));
    assert!(lines[0].contains("duration_ms=\"123\" request_bytes=\"0\" response_bytes=\"4294967296\" close_status=\"1000\" count=\"-2\" error=\"io\""));
    assert!(!lines[0].contains("wrong-session"));
    assert!(!lines[0].contains("sensitive-exception-sentinel"));
    assert!(!lines[0].contains('\u{fffd}'));
    assert!(lines[0].len() < 4_096);
    assert!(lines[1].contains("instance=\"instance-two\""));
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}


#[test]
fn timestamps_keep_utc_offsets_seven_fraction_digits_and_gregorian_dates() {
    let directory = TempDirectory::new("timestamp");
    let path = directory.path().join("diagnostics.log");
    let fixed = UNIX_EPOCH + Duration::new(951_782_400, 123_456_700);
    let log = FileDiagnosticLog::open_session_with_clock(
        &path,
        "instance",
        "timestamp",
        |_| {},
        move || fixed,
    )
    .unwrap();
    log.write(&event("clock"));
    let output = fs::read_to_string(path).unwrap();
    assert!(output.starts_with("2000-02-29T00:00:00.1234567+00:00 INFO"));
}

#[test]
fn byte_counts_preserve_zero_large_values_and_omit_unknown_sizes() {
    let directory = TempDirectory::new("bytes");
    let path = directory.path().join("diagnostics.log");
    let log = FileDiagnosticLog::open_session_with_clock(
        &path,
        "instance",
        "byte-counts",
        |_| {},
        || UNIX_EPOCH,
    )
    .unwrap();
    let mut large = event("request_size");
    large.request_bytes = Some(4_294_967_296);
    log.write(&large);
    let mut zero = event("request_finished");
    zero.request_bytes = Some(0);
    zero.response_bytes = Some(0);
    log.write(&zero);
    log.write(&event("request_finished"));
    let lines: Vec<_> = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("request_bytes=\"4294967296\""));
    assert!(!lines[0].contains("response_bytes="));
    assert!(lines[1].contains("request_bytes=\"0\"") && lines[1].contains("response_bytes=\"0\""));
    assert!(!lines[2].contains("request_bytes=") && !lines[2].contains("response_bytes="));
}

#[test]
fn concurrent_global_writers_have_distinct_files_intact_lines_and_collision_safety() {
    let directory = TempDirectory::new("global");
    let first_id = FileDiagnosticLog::create_instance_id();
    let second_id = FileDiagnosticLog::create_instance_id();
    assert_ne!(first_id, second_id);
    let first =
        Arc::new(FileDiagnosticLog::open_global(directory.path(), &first_id, |_| {}).unwrap());
    let second =
        Arc::new(FileDiagnosticLog::open_global(directory.path(), &second_id, |_| {}).unwrap());
    assert_eq!(first.scope(), DiagnosticScope::Global);
    thread::scope(|scope| {
        for _ in 0..4 {
            let first = Arc::clone(&first);
            let second = Arc::clone(&second);
            scope.spawn(move || {
                for index in 0..50 {
                    let mut event =
                        DiagnosticEvent::new("process", "first", DiagnosticSeverity::Information);
                    event.count = Some(index);
                    first.write(&event);
                    let mut event =
                        DiagnosticEvent::new("process", "second", DiagnosticSeverity::Warning);
                    event.count = Some(index);
                    second.write(&event);
                }
            });
        }
    });
    let files: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|item| item.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    for path in files {
        let lines: Vec<_> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 200);
        assert_eq!(
            lines.iter().collect::<std::collections::HashSet<_>>().len(),
            200
        );
        assert!(
            lines
                .iter()
                .all(|line| line.ends_with('"') && line.contains(" count=\""))
        );
    }
    let warning_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let collision = FileDiagnosticLog::open_global(
        directory.path(),
        &first_id,
        warning_counter(Arc::clone(&warning_count)),
    )
    .unwrap();
    assert!(collision.is_disabled());
    collision.write(&event("must-not-overwrite"));
    assert_eq!(warning_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let first_path = directory.path().join(format!("parrot-{first_id}.log"));
    assert_eq!(fs::read_to_string(first_path).unwrap().lines().count(), 200);
}

#[test]
fn rotation_preserves_order_and_disables_nonfatally_on_failure() {
    let directory = TempDirectory::new("rotation");
    let path = directory.path().join("diagnostics.log");
    fs::write(&path, vec![b'x'; 10 * 1024 * 1024]).unwrap();
    fs::write(path.with_extension("log.1"), "previous-one").unwrap();
    fs::write(path.with_extension("log.2"), "previous-two").unwrap();
    fs::write(path.with_extension("log.3"), "retired").unwrap();
    let log = FileDiagnosticLog::open_session_with_clock(
        &path,
        "instance",
        "rotation",
        |_| {},
        || UNIX_EPOCH,
    )
    .unwrap();
    log.write(&event("first"));
    log.write(&event("second"));
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert_eq!(
        fs::read_to_string(path.with_extension("log.2")).unwrap(),
        "previous-one"
    );
    assert_eq!(
        fs::read_to_string(path.with_extension("log.3")).unwrap(),
        "previous-two"
    );
    for item in fs::read_dir(directory.path()).unwrap() {
        assert!(item.unwrap().metadata().unwrap().len() <= 10 * 1024 * 1024);
    }

    let obstructed_dir = TempDirectory::new("rotation-obstructed");
    let obstructed = obstructed_dir.path().join("diagnostics.log");
    fs::write(&obstructed, vec![b'x'; 10 * 1024 * 1024]).unwrap();
    fs::create_dir(obstructed.with_extension("log.3")).unwrap();
    let warning_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let blocked = FileDiagnosticLog::open_session_with_clock(
        &obstructed,
        "instance",
        "rotation-obstructed",
        warning_counter(Arc::clone(&warning_count)),
        || UNIX_EPOCH,
    )
    .unwrap();
    blocked.write(&event("first"));
    blocked.write(&event("second"));
    assert!(blocked.is_disabled());
    assert_eq!(warning_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(fs::metadata(&obstructed).unwrap().len(), 10 * 1024 * 1024);

    let obstructed_one_dir = TempDirectory::new("rotation-obstructed-one");
    let obstructed_one = obstructed_one_dir.path().join("diagnostics.log");
    fs::write(&obstructed_one, vec![b'x'; 10 * 1024 * 1024]).unwrap();
    fs::create_dir(obstructed_one.with_extension("log.1")).unwrap();
    let warning_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let blocked = FileDiagnosticLog::open_session_with_clock(
        &obstructed_one,
        "instance",
        "rotation-obstructed-one",
        warning_counter(Arc::clone(&warning_count)),
        || UNIX_EPOCH,
    )
    .unwrap();
    blocked.write(&event("first"));
    blocked.write(&event("second"));
    assert!(blocked.is_disabled());
    assert_eq!(warning_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        fs::metadata(&obstructed_one).unwrap().len(),
        10 * 1024 * 1024
    );
    assert!(obstructed_one.with_extension("log.1").is_dir());
}

#[test]
fn open_and_write_failures_are_nonfatal_and_warn_once_without_paths() {
    let directory = TempDirectory::new("failures");
    let path = directory.path().join("diagnostics.log");
    fs::create_dir(&path).unwrap();
    let warning_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed = FileDiagnosticLog::open_session(
        &path,
        "instance",
        "failure",
        warning_counter(Arc::clone(&warning_count)),
    )
    .unwrap();
    failed.write(&event("first"));
    failed.write(&event("second"));
    assert!(failed.is_disabled());
    assert_eq!(warning_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let full = directory.path().join("full.log");
        symlink("/dev/full", &full).unwrap();
        let write_warning_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failed = FileDiagnosticLog::open_session(
            &full,
            "instance",
            "failure",
            warning_counter(Arc::clone(&write_warning_count)),
        )
        .unwrap();
        failed.write(&event("first"));
        failed.write(&event("second"));
        assert!(failed.is_disabled());
        assert_eq!(
            write_warning_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}

#[test]
fn operation_outcomes_log_only_metadata_not_payload_or_exception_text() {
    let directory = TempDirectory::new("outcomes");
    let path = directory.path().join("diagnostics.log");
    let log = FileDiagnosticLog::open_session_with_clock(
        &path,
        "test",
        "user-session-diagnostics",
        |_| {},
        SystemTime::now,
    )
    .unwrap();
    for (outcome, expected) in [
        ("completed", "completed"),
        ("failed", "failed"),
        ("cancelled", "cancelled"),
        ("unknown_tool", "unknown_tool"),
        ("turn_failed", "failed"),
        ("turn_cancelled", "cancelled"),
        ("multiple_tools", "completed"),
        ("runaway", "completed"),
    ] {
        let mut entry = DiagnosticEvent::new("tool", "finished", DiagnosticSeverity::Information);
        entry.correlation_id = Some("call-diagnostic".into());
        entry.outcome = Some(expected.into());
        entry.error_code = if outcome == "runaway" {
            Some("runaway".into())
        } else {
            None
        };
        // The event schema has no body/exception-message field; callers report
        // classified codes and safe correlation metadata only.
        log.write(&entry);
    }
    let output = fs::read_to_string(path).unwrap();
    assert_eq!(output.lines().count(), 8);
    assert!(output.contains("category=\"tool\" event=\"finished\""));
    assert!(output.contains("category=\"tool\" event=\"finished\" instance=\"test\" session=\"user-session-diagnostics\""));
    assert!(output.contains("correlation=\"call-diagnostic\""));
    assert!(output.contains("error=\"runaway\""));
}

#[test]
fn user_session_scope_overrides_untrusted_event_session_and_stays_on_selected_tree() {
    let directory = TempDirectory::new("selected-tree");
    let current = directory.path().join("parrotrust").join("session.log");
    let legacy = directory.path().join("parrotdotnet").join("session.log");
    let log = FileDiagnosticLog::open_session_with_clock(
        &legacy,
        "test",
        "selected-legacy-session",
        |_| {},
        || UNIX_EPOCH,
    )
    .unwrap();
    let mut entry = event("partial_initialization_failed");
    entry.user_session_id = Some("duplicate-new-root-session".into());
    let factory_failure = DiagnosticFailure {
        kind: FailureKind::InvalidOperation,
        message: "private-factory-failure-sentinel",
        path: Some("/private/factory-credential-path"),
    };
    entry.error_code = Some(DiagnosticEvent::classify_failure(&factory_failure).into());
    log.write(&entry);
    assert!(legacy.exists());
    assert!(!current.exists());
    let output = fs::read_to_string(legacy).unwrap();
    assert!(output.contains("session=\"selected-legacy-session\""));
    assert!(!output.contains("duplicate-new-root-session"));
    assert!(output.contains("event=\"partial_initialization_failed\""));
    assert!(output.contains("error=\"invalid_operation\""));
    assert!(!output.contains("private-factory-failure-sentinel"));
    assert!(!output.contains("/private/factory-credential-path"));
}

#[test]
fn invalid_global_instance_ids_are_rejected_before_path_construction() {
    let directory = TempDirectory::new("invalid-id");
    let error = FileDiagnosticLog::open_global(directory.path(), "../secret", |_| {}).unwrap_err();
    assert_eq!(error, InvalidDiagnosticInstanceId);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn failure_classifier_uses_only_fixed_safe_codes() {
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Cancelled)),
        "cancelled"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::AccessDenied)),
        "access_denied"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Io)),
        "io"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Timeout)),
        "timeout"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Http)),
        "http"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Argument)),
        "argument"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::InvalidOperation)),
        "invalid_operation"
    );
    assert_eq!(
        DiagnosticEvent::classify_failure(&failure(FailureKind::Unexpected)),
        "unexpected"
    );
}
