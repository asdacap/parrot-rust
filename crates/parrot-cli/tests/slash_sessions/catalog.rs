use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use parrot_cli::slash_sessions::{
    CallbackSessionBinding, CommandContext, ModeCommand, ModeSelection, ModesCommand,
    RpcSlashSession, SessionsCommand, SkillRefresh, SlashActivity, SlashCommand, SlashFailure,
    SlashSession, StatusCommand,
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::config::Configuration;
use protobuf::Message;
use tokio::sync::Notify;
use tonic::{Code, Status};

use crate::native_fixture::{Fixture, RecordingDialog, Stop};

struct Activity {
    waits: AtomicUsize,
    trace: Arc<Mutex<Vec<String>>>,
}
impl Activity {
    fn new(trace: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            waits: AtomicUsize::new(0),
            trace,
        }
    }
}
impl SlashActivity for Activity {
    fn wait_until_idle<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            c.check()
                .map_err(|_| SlashFailure::Rpc(Status::cancelled("fixture cancelled")))?;
            self.waits.fetch_add(1, Ordering::SeqCst);
            self.trace.lock().unwrap().push("idle".into());
            Ok(())
        })
    }
}

fn session(fixture: &mut Fixture) -> RpcSlashSession {
    assert!(fixture.root.is_dir());
    RpcSlashSession::new(
        fixture.client(),
        wire::UserSession {
            id: "session-0".into(),
            model: "provider/current".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(Mutex::new(
            Configuration::from_content(
                "",
                &BTreeMap::from([("HOME".into(), fixture.root.to_string_lossy().into_owned())]),
                |_| Ok(()),
            )
            .unwrap(),
        )),
        true,
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(async { Ok(()) })
        }))),
    )
}
fn refresh(trace: Arc<Mutex<Vec<String>>>, failure: bool) -> SkillRefresh {
    Arc::new(move |_| {
        let trace = trace.clone();
        Box::pin(async move {
            trace.lock().unwrap().push("refresh".into());
            if failure {
                Err(SlashFailure::Command("refresh failed".into()))
            } else {
                Ok(())
            }
        })
    })
}
async fn run(
    command: &dyn SlashCommand,
    session: &mut RpcSlashSession,
    dialog: &mut RecordingDialog,
    activity: &dyn SlashActivity,
    refresh: &SkillRefresh,
    c: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    command
        .run(
            "ignored arguments",
            &mut CommandContext {
                session,
                activity,
                dialog,
                refresh_skills: refresh,
                commands: &[],
            },
            c,
        )
        .await
}
fn modes() -> wire::ListModesResponse {
    wire::ListModesResponse {
        modes: ["build", "query", "plan"]
            .into_iter()
            .map(|id| wire::Mode {
                id: id.into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
fn rpc_code(result: Result<(), SlashFailure>) -> Code {
    match result {
        Err(SlashFailure::Rpc(status)) => status.code(),
        other => panic!("expected original RPC failure, got {other:?}"),
    }
}

#[tokio::test]
async fn mode_selects_then_waits_updates_refreshes_and_shows() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let observed = trace.clone();
    let mut fixture = Fixture::start(move |method, bytes| match method {
        "list_modes" => Ok(modes().write_to_bytes().unwrap()),
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(bytes).unwrap();
            assert_eq!(request.user_session_id, "session-0");
            assert_eq!(request.mode, "plan");
            assert!(request.model.is_empty());
            observed.lock().unwrap().push("update".into());
            Ok(wire::UserSession {
                id: request.user_session_id,
                model: "provider/current".into(),
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => Err(Status::unimplemented("unused catalog fixture operation")),
    })
    .await;
    let command = ModeCommand::new(ModeSelection::new(fixture.client()));
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog {
        trace: trace.clone(),
        ..Default::default()
    };
    dialog.selections.push_back(Some("plan".into()));
    let activity = Activity::new(trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(session.mode(), "plan");
    assert_eq!(session.id(), "session-0");
    assert_eq!(activity.waits.load(Ordering::SeqCst), 1);
    assert_eq!(dialog.shown, ["mode is now plan"]);
    assert_eq!(dialog.pickers[0].0, "Select a mode");
    assert_eq!(
        dialog.pickers[0]
            .1
            .iter()
            .map(|option| (&*option.id, &*option.label, &*option.description))
            .collect::<Vec<_>>(),
        [
            ("build", "build", "Session mode"),
            ("query", "query", "Session mode"),
            ("plan", "plan", "Session mode")
        ]
    );
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "loading:Loading modes…",
            "loaded:Loading modes…",
            "select:Select a mode",
            "idle",
            "update",
            "refresh",
            "show:mode is now plan"
        ]
    );
    fixture.close().await;
}

#[tokio::test]
async fn mode_dismissal_does_not_wait_update_or_refresh() {
    let mut fixture = Fixture::start(|method, _| {
        assert_eq!(method, "list_modes");
        Ok(modes().write_to_bytes().unwrap())
    })
    .await;
    let command = ModeCommand::new(ModeSelection::new(fixture.client()));
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(session.mode(), "build");
    assert_eq!(activity.waits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert!(dialog.shown.is_empty());
    assert!(
        !dialog
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|item| item == "refresh")
    );
    fixture.close().await;
}

#[tokio::test]
async fn empty_modes_report_error_without_picker_or_wait() {
    let mut fixture =
        Fixture::start(|_, _| Ok(wire::ListModesResponse::new().write_to_bytes().unwrap())).await;
    let command = ModeCommand::new(ModeSelection::new(fixture.client()));
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(dialog.errors, ["no modes are available"]);
    assert!(dialog.pickers.is_empty());
    assert_eq!(activity.waits.load(Ordering::SeqCst), 0);
    assert_eq!(session.mode(), "build");
    fixture.close().await;
}

#[tokio::test]
async fn modes_list_preserves_server_order() {
    let mut fixture = Fixture::start(|method, _| {
        assert_eq!(method, "list_modes");
        Ok(modes().write_to_bytes().unwrap())
    })
    .await;
    let command = ModesCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(dialog.shown, ["build", "query", "plan"]);
    assert_eq!(dialog.loads, ["Loading modes…"]);
    assert!(dialog.pickers.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn mode_update_failure_preserves_session_and_skips_refresh() {
    let mut fixture = Fixture::start(|method, _| match method {
        "list_modes" => Ok(modes().write_to_bytes().unwrap()),
        "update_session" => Err(Status::permission_denied("update rejected")),
        _ => Err(Status::unimplemented("unused catalog fixture operation")),
    })
    .await;
    let command = ModeCommand::new(ModeSelection::new(fixture.client()));
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("plan".into()));
    let activity = Activity::new(dialog.trace.clone());
    assert_eq!(
        rpc_code(
            run(
                &command,
                &mut session,
                &mut dialog,
                &activity,
                &refresh(activity.trace.clone(), false),
                &Stop::new()
            )
            .await
        ),
        Code::PermissionDenied
    );
    assert_eq!(session.mode(), "build");
    assert!(dialog.shown.is_empty());
    assert!(
        !dialog
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|item| item == "refresh")
    );
    fixture.close().await;
}

#[tokio::test]
async fn mode_refresh_failure_keeps_successful_update_without_success_message() {
    let mut fixture = Fixture::start(|method, bytes| match method {
        "list_modes" => Ok(modes().write_to_bytes().unwrap()),
        "update_session" => {
            let request = wire::UpdateSessionRequest::parse_from_bytes(bytes).unwrap();
            Ok(wire::UserSession {
                id: request.user_session_id,
                model: "provider/current".into(),
                mode: request.mode,
                ..Default::default()
            }
            .write_to_bytes()
            .unwrap())
        }
        _ => Err(Status::unimplemented("unused catalog fixture operation")),
    })
    .await;
    let command = ModeCommand::new(ModeSelection::new(fixture.client()));
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("plan".into()));
    let activity = Activity::new(dialog.trace.clone());
    let result = run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), true),
        &Stop::new(),
    )
    .await;
    assert!(matches!(result, Err(SlashFailure::Command(message)) if message == "refresh failed"));
    assert_eq!(session.mode(), "plan");
    assert!(dialog.shown.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn sessions_lists_source_order_markers_placeholders_and_states() {
    let mut fixture = Fixture::start(|method, bytes| {
        assert_eq!(method, "list_sessions");
        assert!(
            wire::ListSessionsRequest::parse_from_bytes(bytes)
                .unwrap()
                .working_directory
                .is_empty()
        );
        let item = |id: &str, name: &str, model: &str, created: &str, state| wire::SessionSummary {
            user_session_id: id.into(),
            root_agent_name: name.into(),
            model: model.into(),
            created_at: created.into(),
            state,
            ..Default::default()
        };
        Ok(wire::ListSessionsResponse {
            sessions: vec![
                item(
                    "session-later",
                    "main-2",
                    "provider/later",
                    "2026-07-28T02:00:00Z",
                    wire::SessionState::SESSION_STATE_INACTIVE.into(),
                ),
                item(
                    "session-0",
                    "main",
                    "provider/current",
                    "2026-07-28T01:00:00Z",
                    wire::SessionState::SESSION_STATE_ACTIVE.into(),
                ),
                item(
                    "session-legacy",
                    "",
                    "provider/legacy",
                    "2026-07-28T03:00:00Z",
                    wire::SessionState::SESSION_STATE_INACTIVE.into(),
                ),
                item(
                    "session-corrupt",
                    "",
                    "",
                    "",
                    wire::SessionState::SESSION_STATE_CORRUPT.into(),
                ),
            ],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let command = SessionsCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.shown.join("|"),
        "  <unnamed>  session-corrupt  <unknown>  corrupt|* main  session-0  provider/current  active|  main-2  session-later  provider/later  inactive|  <unnamed>  session-legacy  provider/legacy  inactive"
    );
    assert_eq!(dialog.loads, ["Loading sessions…"]);
    assert_eq!(session.id(), "session-0");
    fixture.close().await;
}

#[tokio::test]
async fn sessions_use_ordinal_text_order_and_id_ties_with_unknown_enum() {
    let mut fixture = Fixture::start(|_, _| {
        let item = |id: &str, created: &str| wire::SessionSummary {
            user_session_id: id.into(),
            created_at: created.into(),
            state: protobuf::EnumOrUnknown::from_i32(999),
            ..Default::default()
        };
        Ok(wire::ListSessionsResponse {
            sessions: vec![
                item("z", "a"),
                item("\u{e000}", "a"),
                item("\u{10000}", "a"),
                item("first", "A"),
            ],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let command = SessionsCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.shown,
        [
            "  <unnamed>  first  <unknown>  unknown",
            "  <unnamed>  z  <unknown>  unknown",
            "  <unnamed>  \u{10000}  <unknown>  unknown",
            "  <unnamed>  \u{e000}  <unknown>  unknown"
        ]
    );
    fixture.close().await;
}

#[tokio::test]
async fn sessions_unimplemented_reports_unavailable_without_fallback() {
    let mut fixture = Fixture::start(|_, _| Err(Status::unimplemented("old server"))).await;
    let command = SessionsCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.errors,
        ["session listing is unavailable on this server"]
    );
    assert!(dialog.shown.is_empty());
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert!(
        dialog
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|item| item == "loaded:Loading sessions…")
    );
    fixture.close().await;
}

#[tokio::test]
async fn sessions_other_rpc_failures_and_rpc_cancellation_propagate() {
    for code in [Code::PermissionDenied, Code::NotFound, Code::Cancelled] {
        let mut fixture =
            Fixture::start(move |_, _| Err(Status::new(code, "original details"))).await;
        let command = SessionsCommand::new(fixture.client());
        let mut session = session(&mut fixture);
        let mut dialog = RecordingDialog::default();
        let activity = Activity::new(dialog.trace.clone());
        assert_eq!(
            rpc_code(
                run(
                    &command,
                    &mut session,
                    &mut dialog,
                    &activity,
                    &refresh(activity.trace.clone(), false),
                    &Stop::new()
                )
                .await
            ),
            code
        );
        assert!(dialog.errors.is_empty());
        assert!(dialog.shown.is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn status_prints_raw_status_then_usage_for_current_session() {
    let mut fixture = Fixture::start(|method, bytes| {
        assert_eq!(method, "session_status");
        assert_eq!(
            wire::SessionStatusRequest::parse_from_bytes(bytes)
                .unwrap()
                .user_session_id,
            "session-0"
        );
        Ok(wire::SessionStatusResponse {
            status: "Model: provider/model\nraw\u{1b}[31m".into(),
            usage_lines: vec!["Credits: 5.00".into(), "".into()],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap())
    })
    .await;
    let command = StatusCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.printed,
        ["Model: provider/model\nraw\u{1b}[31m", "Credits: 5.00", ""]
    );
    assert!(dialog.shown.is_empty());
    assert_eq!(dialog.loads, ["Loading status…"]);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    let call = fixture.calls.lock().unwrap()[0].clone();
    assert_eq!(call.method, "session_status");
    assert_eq!(
        wire::SessionStatusRequest::parse_from_bytes(&call.body)
            .unwrap()
            .user_session_id,
        "session-0"
    );
    fixture.close().await;
}

#[tokio::test]
async fn status_empty_response_prints_placeholder() {
    let mut fixture =
        Fixture::start(|_, _| Ok(wire::SessionStatusResponse::new().write_to_bytes().unwrap()))
            .await;
    let command = StatusCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh(activity.trace.clone(), false),
        &Stop::new(),
    )
    .await
    .unwrap();
    assert_eq!(dialog.printed, ["no status is currently available"]);
    fixture.close().await;
}

#[tokio::test]
async fn status_unimplemented_and_not_found_have_exact_errors() {
    for (code, expected) in [
        (
            Code::Unimplemented,
            "session status is unavailable on this server",
        ),
        (
            Code::NotFound,
            "no status is available yet for this session",
        ),
    ] {
        let mut fixture =
            Fixture::start(move |_, _| Err(Status::new(code, "native details"))).await;
        let command = StatusCommand::new(fixture.client());
        let mut session = session(&mut fixture);
        let mut dialog = RecordingDialog::default();
        let activity = Activity::new(dialog.trace.clone());
        run(
            &command,
            &mut session,
            &mut dialog,
            &activity,
            &refresh(activity.trace.clone(), false),
            &Stop::new(),
        )
        .await
        .unwrap();
        assert_eq!(dialog.errors, [expected]);
        assert!(dialog.printed.is_empty());
        assert!(dialog.shown.is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn status_other_rpc_failures_and_rpc_cancellation_propagate() {
    for code in [Code::PermissionDenied, Code::Cancelled] {
        let mut fixture =
            Fixture::start(move |_, _| Err(Status::new(code, "native details"))).await;
        let command = StatusCommand::new(fixture.client());
        let mut session = session(&mut fixture);
        let mut dialog = RecordingDialog::default();
        let activity = Activity::new(dialog.trace.clone());
        assert_eq!(
            rpc_code(
                run(
                    &command,
                    &mut session,
                    &mut dialog,
                    &activity,
                    &refresh(activity.trace.clone(), false),
                    &Stop::new()
                )
                .await
            ),
            code
        );
        assert!(dialog.errors.is_empty());
        assert!(dialog.printed.is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn status_caller_cancellation_during_native_rpc_propagates_and_balances_load() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let signal_started = started.clone();
    let signal_release = release.clone();
    let mut fixture = Fixture::start_async(move |method, _| {
        assert_eq!(method, "session_status");
        let started = signal_started.clone();
        let release = signal_release.clone();
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Ok(wire::SessionStatusResponse::new().write_to_bytes().unwrap())
        })
    })
    .await;
    let command = StatusCommand::new(fixture.client());
    let mut session = session(&mut fixture);
    let mut dialog = RecordingDialog::default();
    let activity = Activity::new(dialog.trace.clone());
    let refresh = refresh(activity.trace.clone(), false);
    let cancellation = Stop::new();
    let mut operation = Box::pin(run(
        &command,
        &mut session,
        &mut dialog,
        &activity,
        &refresh,
        &cancellation,
    ));
    tokio::select! {
        result = &mut operation => panic!("native request unexpectedly finished: {result:?}"),
        () = started.notified() => {},
    }
    cancellation.cancel();
    assert_eq!(
        rpc_code(
            tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
        ),
        Code::Cancelled
    );
    release.notify_one();
    assert!(dialog.errors.is_empty());
    assert!(dialog.printed.is_empty());
    assert_eq!(dialog.loads, ["Loading status…"]);
    assert!(
        dialog
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|item| item == "loaded:Loading status…")
    );
    assert_eq!(session.id(), "session-0");
    tokio::time::timeout(Duration::from_secs(5), fixture.close())
        .await
        .unwrap();
}
