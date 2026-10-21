use std::sync::{Arc, Mutex};

use parrot_cli::presenters::{EnhancedSessionPresenter, SessionReplayContext};
use parrot_domain::protocol::messages::parrot::{self as wire, event::Payload};
use protobuf::MessageField;

use super::session::queue_event;
use super::turn::{child, commits, completed, ended, event, start, text};

#[test]
fn replay_context_ends_at_first_usage_snapshot_and_never_reenters() {
    let mut context = SessionReplayContext::new(true);
    assert!(context.is_replaying());
    let events = [
        start(""),
        queue_event("", Vec::new()),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
        ),
        start(""),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
        ),
    ];
    for (published, expected) in events.iter().zip([true, true, false, false, false]) {
        context.observe(published);
        assert_eq!(context.is_replaying(), expected);
    }
}

#[test]
fn live_context_never_enters_replay() {
    let mut context = SessionReplayContext::new(false);
    context.observe(&start(""));
    assert!(!context.is_replaying());
    context.observe(&event(
        "",
        Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
    ));
    assert!(!context.is_replaying());
}

fn replay_completion_case(tail: Payload, expected_busy: bool) {
    let mut context = SessionReplayContext::new(true);
    let states = Arc::new(Mutex::new(Vec::new()));
    let captured = states.clone();
    context.set_completion(Arc::new(move |busy| captured.lock().unwrap().push(busy)));
    let events = [
        child("root"),
        start("child"),
        start("root"),
        completed("root"),
        start("root"),
        event("root", tail),
        start("child"),
        event("child", Payload::TurnFailed(wire::TurnFailed::default())),
    ];
    for published in events {
        context.observe(&published);
        assert!(context.is_replaying());
        assert!(states.lock().unwrap().is_empty());
    }
    context.observe(&event(
        "",
        Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
    ));
    assert!(!context.is_replaying());
    assert_eq!(*states.lock().unwrap(), [expected_busy]);
    for published in [
        start("root"),
        ended("root"),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
        ),
    ] {
        context.observe(&published);
    }
    assert_eq!(*states.lock().unwrap(), [expected_busy]);
}
#[test]
fn replay_completion_after_root_started_is_busy_once() {
    replay_completion_case(Payload::TurnStarted(wire::TurnStarted::default()), true);
}
#[test]
fn replay_completion_after_root_ended_is_idle_once() {
    replay_completion_case(Payload::TurnEnded(wire::TurnEnded::default()), false);
}
#[test]
fn replay_completion_after_root_mode_completed_is_idle_once() {
    replay_completion_case(
        Payload::ModeTurnCompleted(wire::ModeTurnCompleted::default()),
        false,
    );
}
#[test]
fn replay_completion_after_root_failed_is_idle_once() {
    replay_completion_case(Payload::TurnFailed(wire::TurnFailed::default()), false);
}

fn history_and_live(failed: bool) -> Vec<wire::Event> {
    vec![
        start("root"),
        event(
            "root",
            Payload::PlanCompleted(wire::PlanCompleted {
                markdown: "old plan report".into(),
                dialog: MessageField::some(wire::TurnCompleteDialog {
                    prompt: "old plan dialog".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ),
        event(
            "root",
            Payload::PermissionPending(wire::PendingPermission {
                id: "old-permission".into(),
                ..Default::default()
            }),
        ),
        completed("root"),
        start("root"),
        event(
            "root",
            Payload::TurnFailed(wire::TurnFailed {
                message: "old failure".into(),
                ..Default::default()
            }),
        ),
        start("root"),
        text("root", "history after failed turn"),
        completed("root"),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
        ),
        start("root"),
        event(
            "root",
            Payload::PermissionPending(wire::PendingPermission {
                id: "live-permission".into(),
                ..Default::default()
            }),
        ),
        event(
            "root",
            Payload::PlanCompleted(wire::PlanCompleted {
                markdown: "live plan report".into(),
                ..Default::default()
            }),
        ),
        if failed {
            event(
                "root",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "live failure".into(),
                    ..Default::default()
                }),
            )
        } else {
            completed("root")
        },
    ]
}
fn enhanced_replay_case(failed: bool) {
    let mut presenter = EnhancedSessionPresenter::new(160, false, true);
    let mut output = String::new();
    let mut errors = String::new();
    let mut live_plans = Vec::new();
    let mut permissions = Vec::new();
    let mut started = 0;
    let mut finished = 0;
    let mut completion = None;
    for published in history_and_live(failed) {
        let update = presenter.observe(&published).unwrap();
        output.push_str(&commits(&update.commits, 160));
        errors.push_str(&update.errors.join("\n"));
        if let Some(plan) = update.complete_plan {
            live_plans.push(plan.markdown);
        }
        if update.live_event {
            if let Some(Payload::PermissionPending(permission)) = &published.payload {
                permissions.push(permission.id.clone());
            }
        }
        started += usize::from(update.started_main_turn);
        finished += usize::from(update.finished_main_turn);
        completion = update.completion.or(completion);
    }
    assert_eq!(completion, Some(!failed));
    assert!(output.contains("old plan report"));
    assert!(output.contains("history after failed turn"));
    assert!(!output.contains("old plan dialog"));
    assert!(errors.contains("old failure"));
    assert_eq!(live_plans, ["live plan report"]);
    assert_eq!(permissions, ["live-permission"]);
    assert_eq!(started, 1);
    assert_eq!(finished, 1);
    if failed {
        assert!(errors.contains("live failure"));
    }
}
#[test]
fn enhanced_replay_preserves_history_and_only_live_action_flags_on_success() {
    enhanced_replay_case(false);
}
#[test]
fn enhanced_replay_preserves_history_and_only_live_action_flags_on_failure() {
    enhanced_replay_case(true);
}

#[test]
fn presenter_consumes_the_same_external_replay_context_without_duplicate_completion() {
    let context = Arc::new(Mutex::new(SessionReplayContext::new(true)));
    let states = Arc::new(Mutex::new(Vec::new()));
    let captured = states.clone();
    context
        .lock()
        .unwrap()
        .set_completion(Arc::new(move |busy| captured.lock().unwrap().push(busy)));
    let mut presenter = EnhancedSessionPresenter::with_replay_context(80, false, context.clone());
    let root = start("root");
    context.lock().unwrap().observe(&root);
    let historical = presenter.observe(&root).unwrap();
    assert!(!historical.started_main_turn);
    let sentinel = event(
        "",
        Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot::default()),
    );
    context.lock().unwrap().observe(&sentinel);
    let transition = presenter.observe(&sentinel).unwrap();
    assert_eq!(transition.replay_completed, Some(true));
    assert_eq!(*states.lock().unwrap(), [true]);
    let repeated = presenter.observe(&sentinel).unwrap();
    assert_eq!(repeated.replay_completed, None);
    assert_eq!(*states.lock().unwrap(), [true]);
    let now_live = presenter.observe(&start("root")).unwrap();
    assert!(now_live.started_main_turn);
}
