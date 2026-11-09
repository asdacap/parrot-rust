use super::support::{Activity, Dialog, NEVER, Session, Stop, run_with_session};
use parrot_cli::{slash_sessions::SlashFailure, slash_settings::FastCommand};
use tonic::{Code, Status};

#[tokio::test]
async fn fast_command_toggles_and_honors_explicit_choices_without_changing_model_or_effort() {
    let mut session = Session::new("chatgpt/gpt-6.1-sol/high");
    let activity = Activity::default();
    for (arguments, expected) in [
        ("", true),
        ("", false),
        ("on", true),
        ("on", true),
        ("off", false),
    ] {
        let mut dialog = Dialog::default();
        run_with_session(
            &FastCommand,
            arguments,
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(session.current.fast_mode, expected);
        assert_eq!(session.current.model, "chatgpt/gpt-6.1-sol/high");
        assert!(session.selected_models.is_empty());
        assert_eq!(activity.waits(), 0);
        assert_eq!(dialog.shown.len(), 1);
        let state = if expected { "on" } else { "off" };
        assert_eq!(
            dialog.shown[0],
            format!(
                "Fast mode is {state} (saved; ChatGPT requests only). Fast mode uses more of your subscription allowance."
            )
        );
    }
}

#[tokio::test]
async fn invalid_fast_arguments_do_not_mutate_selection() {
    let mut session = Session::new("chatgpt/gpt-6.1-sol/high");
    session.current.fast_mode = true;
    let mut dialog = Dialog::default();
    run_with_session(
        &FastCommand,
        "on extra",
        &mut session,
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert!(session.current.fast_mode);
    assert_eq!(session.current.model, "chatgpt/gpt-6.1-sol/high");
    assert_eq!(dialog.errors, ["Usage: /fast [on|off]"]);
    assert!(dialog.shown.is_empty());
}

#[tokio::test]
async fn fast_command_propagates_session_failure_and_cancellation_without_confirmation() {
    for arguments in ["", "on"] {
        let mut session = Session::new("chatgpt/gpt-6.1-sol/high");
        session.failure = Some(Status::unavailable("offline"));
        let mut dialog = Dialog::default();
        let failure = run_with_session(
            &FastCommand,
            arguments,
            &mut session,
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap_err();
        assert!(matches!(failure, SlashFailure::Rpc(status) if status.code() == Code::Unavailable));
        assert!(!session.current.fast_mode);
        assert!(dialog.shown.is_empty());
    }
    let stop = Stop::new();
    stop.cancel();
    let mut session = Session::new("chatgpt/gpt-6.1-sol/high");
    let mut dialog = Dialog::default();
    assert!(
        run_with_session(
            &FastCommand,
            "on",
            &mut session,
            &mut dialog,
            &Activity::default(),
            &stop,
        )
        .await
        .is_err()
    );
    assert!(!session.current.fast_mode);
    assert!(dialog.shown.is_empty());
}
