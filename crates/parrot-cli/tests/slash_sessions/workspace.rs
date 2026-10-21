use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use chrono::{Local, TimeZone, Utc};
use parrot_cli::slash_sessions::{
    CommandContext, NavigationFailure, PollingSlashActivity, SlashCommand, SlashFailure,
    WorkspaceNavigation, WorkspaceSessionsCommand, parse_creation_instant, workspace_options,
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use tonic::{Code, Status};

use crate::{
    control_fixtures::{Activity, Record, Session, refresh},
    native_fixture::{RecordingDialog, Stop},
};

struct Navigation {
    listed: wire::ListSessionsResponse,
    loaded: Mutex<Vec<String>>,
    lists: AtomicUsize,
    failure: Option<(bool, usize)>,
}
#[tokio::test]
async fn missing_remote_workspace_reports_original_recoverable_message_without_load() {
    let mut nav = navigation();
    nav.failure = Some((false, 5));
    let nav = Arc::new(nav);
    let (result, dialog, session) = run(nav.clone(), None).await;
    result.unwrap();
    assert_eq!(dialog.errors, ["workspace unavailable"]);
    assert!(nav.loaded.lock().unwrap().is_empty());
    assert!(session.record.events().is_empty());
}
fn failure(kind: usize) -> NavigationFailure {
    match kind {
        0 => Status::unimplemented("old server").into(),
        1 => Status::unavailable("host unavailable").into(),
        2 => Status::cancelled("remote cancelled").into(),
        3 => NavigationFailure::Recoverable("local load failed".into()),
        5 => NavigationFailure::Recoverable("workspace unavailable".into()),
        _ => NavigationFailure::Other("unexpected failure".into()),
    }
}
impl WorkspaceNavigation for Navigation {
    fn list<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<wire::ListSessionsResponse, NavigationFailure>> {
        Box::pin(async move {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if let Some((false, kind)) = self.failure {
                return Err(failure(kind));
            }
            Ok(self.listed.clone())
        })
    }
    fn load<'a>(
        &'a self,
        id: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(async move {
            self.loaded.lock().unwrap().push(id.into());
            if let Some((true, kind)) = self.failure {
                return Err(failure(kind));
            }
            Ok(())
        })
    }
}
fn navigation() -> Navigation {
    Navigation {
        listed: wire::ListSessionsResponse {
            sessions: vec![wire::SessionSummary {
                user_session_id: "selected".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        loaded: Mutex::new(Vec::new()),
        lists: AtomicUsize::new(0),
        failure: None,
    }
}
async fn run(
    nav: Arc<Navigation>,
    selected: Option<&str>,
) -> (Result<(), SlashFailure>, RecordingDialog, Session) {
    let command = WorkspaceSessionsCommand::new(nav);
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(selected.map(str::to_owned));
    let record = Record::default();
    let mut session = Session::new(record.clone());
    let activity = Activity::new(record);
    let refresh = refresh(Record::default());
    let mut context = CommandContext {
        session: &mut session,
        activity: &activity,
        dialog: &mut dialog,
        refresh_skills: &refresh,
        commands: &[],
    };
    let result = command
        .run("arguments are ignored", &mut context, &Stop::new())
        .await;
    (result, dialog, session)
}

#[test]
fn creation_instants_follow_actual_producer_roundtrip_and_preserved_offsets() {
    let instant = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
    for text in [
        "2026-07-28T01:00:00.0000000+00:00",
        "2026-07-28T03:00:00+02:00",
        "2026-07-28T01:00:00Z",
        "2026-07-28T01:00:00",
    ] {
        assert_eq!(parse_creation_instant(text), Some(instant), "{text}");
    }
    for text in ["", "broken", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z"] {
        assert!(parse_creation_instant(text).is_none());
    }
    // Broader invariant non-ISO DateTimeOffset acceptance is explicitly OPEN,
    // not proven malformed/source-equivalent by these actual-protocol tests.
}

#[tokio::test]
async fn workspace_options_sort_instants_then_stable_ids_and_display_local_time() {
    let summary = |id: &str, created: &str, state| wire::SessionSummary {
        user_session_id: id.into(),
        created_at: created.into(),
        state,
        ..Default::default()
    };
    let mut current = summary(
        "session-0",
        "2026-07-28T01:00:00Z",
        wire::SessionState::SESSION_STATE_ACTIVE.into(),
    );
    current.root_agent_name = "main".into();
    let summaries = vec![
        summary(
            "missing",
            "",
            wire::SessionState::SESSION_STATE_CORRUPT.into(),
        ),
        summary("bad", "broken", protobuf::EnumOrUnknown::from_i32(77)),
        summary(
            "older",
            "2026-07-28T00:00:00Z",
            wire::SessionState::SESSION_STATE_INACTIVE.into(),
        ),
        summary(
            "tie-b",
            "2026-07-28T03:00:00+02:00",
            wire::SessionState::SESSION_STATE_ACTIVE.into(),
        ),
        current,
    ];
    let options = workspace_options(&summaries, "session-0");
    let mut nav = navigation();
    nav.listed.sessions = summaries;
    let nav = Arc::new(nav);
    let command = WorkspaceSessionsCommand::new(nav.clone());
    let mut dialog = RecordingDialog::default();
    let record = Record::default();
    let mut session = Session::new(record.clone());
    session.current.id = "session-0".into();
    let activity = Activity::new(record);
    let refresh = refresh(Record::default());
    command
        .run(
            "",
            &mut CommandContext {
                session: &mut session,
                activity: &activity,
                dialog: &mut dialog,
                refresh_skills: &refresh,
                commands: &[],
            },
            &Stop::new(),
        )
        .await
        .unwrap();
    assert_eq!(dialog.pickers.len(), 1);
    assert_eq!(dialog.pickers[0].1, options);
    assert_eq!(nav.lists.load(Ordering::SeqCst), 1);
    assert!(nav.loaded.lock().unwrap().is_empty());
    assert_eq!(
        options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["session-0", "tie-b", "older", "bad", "missing"]
    );
    assert_eq!(options[0].label, "* main  session-0");
    assert_eq!(options[1].label, "  <unnamed>  tie-b");
    let expected = Utc
        .with_ymd_and_hms(2026, 7, 28, 1, 0, 0)
        .unwrap()
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string();
    assert_eq!(options[0].description, format!("active  {expected}"));
    assert!(options[2].description.starts_with("inactive  "));
    assert_eq!(options[3].description, "unknown  unknown creation time");
    assert_eq!(options[4].description, "corrupt  unknown creation time");
}

#[tokio::test]
async fn workspace_empty_current_dismissed_and_corrupt_do_not_load() {
    for selected in [None, Some("session"), Some("corrupt")] {
        let mut nav = navigation();
        nav.listed.sessions = vec![
            wire::SessionSummary {
                user_session_id: "session".into(),
                ..Default::default()
            },
            wire::SessionSummary {
                user_session_id: "corrupt".into(),
                state: wire::SessionState::SESSION_STATE_CORRUPT.into(),
                ..Default::default()
            },
        ];
        let nav = Arc::new(nav);
        let (result, dialog, session) = run(nav.clone(), selected).await;
        result.unwrap();
        assert!(nav.loaded.lock().unwrap().is_empty());
        assert!(session.record.events().is_empty());
        assert_eq!(nav.lists.load(Ordering::SeqCst), 1);
        assert_eq!(session.current.id, "session");
        assert_eq!(
            dialog.errors,
            if selected == Some("corrupt") {
                vec!["session metadata is corrupt"]
            } else {
                vec![]
            }
        );
    }
    let mut nav = navigation();
    nav.listed.sessions.clear();
    let (result, dialog, _) = run(Arc::new(nav), None).await;
    result.unwrap();
    assert_eq!(dialog.shown, ["No sessions in the current workspace."]);
    assert!(dialog.pickers.is_empty());
}

#[tokio::test]
async fn workspace_selected_id_loads_only_after_actual_activity_becomes_idle() {
    let nav = Arc::new(navigation());
    let command = WorkspaceSessionsCommand::new(nav.clone());
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("selected".into()));
    let mut session = Session::new(Record::default());
    let busy = Arc::new(AtomicBool::new(true));
    let observed = busy.clone();
    let activity = PollingSlashActivity(Arc::new(move || observed.load(Ordering::SeqCst)));
    let refresh = refresh(Record::default());
    let stop = Stop::new();
    {
        let mut context = CommandContext {
            session: &mut session,
            activity: &activity,
            dialog: &mut dialog,
            refresh_skills: &refresh,
            commands: &[],
        };
        let running = command.run("", &mut context, &stop);
        tokio::pin!(running);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut running)
                .await
                .is_err()
        );
        assert!(nav.loaded.lock().unwrap().is_empty());
        busy.store(false, Ordering::SeqCst);
        running.await.unwrap();
    }
    assert_eq!(*nav.loaded.lock().unwrap(), ["selected"]);
    assert_eq!(
        dialog.loads,
        ["Loading workspace sessions…", "Loading selected session…"]
    );
    assert_eq!(session.current.id, "session"); // navigation owns committing the actual terminal binding
}

#[tokio::test]
async fn workspace_cancellation_while_busy_preserves_session_and_has_no_error_frame() {
    let nav = Arc::new(navigation());
    let command = WorkspaceSessionsCommand::new(nav.clone());
    let mut dialog = RecordingDialog::default();
    dialog.selections.push_back(Some("selected".into()));
    let mut session = Session::new(Record::default());
    let previous = session.current.clone();
    let activity = PollingSlashActivity(Arc::new(|| true));
    let refresh = refresh(Record::default());
    let stop = Stop::new();
    let result = {
        let mut context = CommandContext {
            session: &mut session,
            activity: &activity,
            dialog: &mut dialog,
            refresh_skills: &refresh,
            commands: &[],
        };
        let running = command.run("", &mut context, &stop);
        tokio::pin!(running);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut running)
                .await
                .is_err()
        );
        stop.cancel();
        running.await
    };
    assert!(matches!(result,Err(SlashFailure::Rpc(status)) if status.code()==Code::Cancelled));
    assert_eq!(session.current, previous);
    assert!(nav.loaded.lock().unwrap().is_empty());
    assert!(dialog.errors.is_empty());
}

#[tokio::test]
async fn workspace_rpc_failures_are_reported_but_rpc_cancellation_propagates() {
    for loading in [false, true] {
        for kind in 0..3 {
            let mut nav = navigation();
            nav.failure = Some((loading, kind));
            let (result, dialog, session) = run(Arc::new(nav), Some("selected")).await;
            if kind == 2 {
                assert!(
                    matches!(result,Err(SlashFailure::Rpc(status))if status.code()==Code::Cancelled)
                );
                assert!(dialog.errors.is_empty());
            } else {
                result.unwrap();
                assert_eq!(
                    dialog.errors,
                    if kind == 0 {
                        vec!["workspace session selection is unavailable on this server"]
                    } else {
                        vec!["host unavailable"]
                    }
                );
            }
            assert_eq!(session.current.id, "session");
        }
    }
}

#[tokio::test]
async fn workspace_expected_local_errors_report_but_unexpected_errors_propagate() {
    for loading in [false, true] {
        for kind in [3, 4] {
            let mut nav = navigation();
            nav.failure = Some((loading, kind));
            let (result, dialog, _) = run(Arc::new(nav), Some("selected")).await;
            if kind == 3 {
                result.unwrap();
                assert_eq!(dialog.errors, ["local load failed"]);
            } else {
                assert!(
                    matches!(result,Err(SlashFailure::Command(message))if message=="unexpected failure")
                );
                assert!(dialog.errors.is_empty());
            }
        }
    }
}
