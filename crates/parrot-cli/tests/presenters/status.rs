use parrot_cli::presenters::EnhancedSessionPresenter;
use parrot_domain::protocol::messages::parrot::{self as wire, event::Payload};

use super::turn::{event, live, start};

fn countdown_case(columns: usize) {
    let mut presenter = EnhancedSessionPresenter::new(columns, false, false);
    presenter
        .set_session("build", "provider/model", false)
        .unwrap();
    let first = presenter.set_question_countdown(Some(61)).unwrap();
    assert!(live(&first.live, columns).contains("Auto-return in 1:01"));
    let next = presenter.set_question_countdown(Some(60)).unwrap();
    assert!(live(&next.live, columns).contains("Auto-return in 1:00"));
    let cleared = presenter.set_question_countdown(None).unwrap();
    assert!(!live(&cleared.live, columns).contains("Auto-return in"));
    // Actual input ownership and preservation are asserted by T72's driver,
    // which owns the input appended after these real presenter rows.
}
#[test]
fn countdown_display_20_columns() {
    countdown_case(20);
}
#[test]
fn countdown_display_80_columns() {
    countdown_case(80);
}

fn awaiting_question_case(remaining: Option<i64>) {
    let mut presenter = EnhancedSessionPresenter::new(160, false, false);
    presenter
        .set_session("build", "provider/model", false)
        .unwrap();
    presenter.observe(&start("root")).unwrap();
    let working = presenter
        .observe(&event(
            "root",
            Payload::ToolStarted(wire::ToolStarted {
                tool_call_id: "question-call".into(),
                tool_name: "question".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(live(&working.live, 160).contains("Working: question"));
    presenter.set_question_countdown(remaining).unwrap();
    let waiting = presenter.set_awaiting_question(true).unwrap();
    let frame = live(&waiting.live, 160);
    assert!(frame.contains("Waiting for your answer…"));
    assert!(!frame.contains("Working: question"));
    assert!(!frame.chars().any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)));
    let resumed = presenter.set_awaiting_question(false).unwrap();
    let frame = live(&resumed.live, 160);
    assert!(!frame.contains("Waiting for your answer…"));
    assert!(frame.contains("Working: question"));
    assert!(frame.chars().any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)));
}
#[test]
fn awaiting_question_without_countdown_hides_spinner_then_restores_activity() {
    awaiting_question_case(None);
}
#[test]
fn awaiting_question_with_countdown_hides_spinner_then_restores_activity() {
    awaiting_question_case(Some(60));
}

#[test]
fn fast_mode_indicator_tracks_session_settings_before_usage_context() {
    let mut presenter = EnhancedSessionPresenter::new(160, false, false);
    presenter
        .observe(&event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                context_size: 50,
                context_limit: 1000,
                ..Default::default()
            }),
        ))
        .unwrap();
    for (model, enabled, expected) in [
        ("provider/model", false, "provider/model (50/1k)"),
        ("provider/model", true, "provider/model ⚡ (50/1k)"),
        ("provider/model", true, "provider/model ⚡ (50/1k)"),
        ("provider/other", true, "provider/other ⚡ (50/1k)"),
        ("provider/other", false, "provider/other (50/1k)"),
    ] {
        let update = presenter.set_session("build", model, enabled).unwrap();
        let output = live(&update.live, 160);
        assert!(output.contains(expected), "{output:?}");
        assert_eq!(output.matches('⚡').count(), usize::from(enabled));
    }
}

#[test]
fn modeline_is_before_selected_countdown_and_ready_clears_preparing_activity() {
    let mut presenter = EnhancedSessionPresenter::new(80, false, false);
    presenter
        .set_session("build", "provider/model", false)
        .unwrap();
    let preparing = presenter.begin_turn().unwrap();
    assert!(live(&preparing.live, 80).contains("Preparing turn…"));
    let busy = presenter.ready(true).unwrap();
    assert!(live(&busy.live, 80).contains("Preparing turn…"));
    let idle = presenter.ready(false).unwrap();
    assert!(!live(&idle.live, 80).contains("Preparing turn…"));
    let selected = presenter.set_question_countdown(Some(61)).unwrap();
    let frame = live(&selected.live, 80);
    assert!(frame.find("mode: build").unwrap() < frame.find("Auto-return in 1:01").unwrap());
}
