mod checkpoint;
mod compact;
mod image;
mod permission;
mod reminders;
mod status;
mod support;

use parrot_domain::contracts::tools::{ParallelSafety, Tool};
use parrot_tools::session::{Invocation, Kind};
use support::{Fixture, definition};

#[test]
fn injected_definitions_are_unchanged_and_all_session_tools_default_to_sequential_and_disabled_after_interrupt()
 {
    let fixture = Fixture::new();
    let invocation = Invocation {
        call_id: "definition-probe".into(),
        assistant_sequence: 1,
        arguments_json: "{}".into(),
        prompt_templates: None,
    };
    for kind in [
        Kind::SetCheckpoint,
        Kind::SetExitReminder,
        Kind::ClearExitReminder,
        Kind::Status,
        Kind::CompactContext,
    ] {
        let tool = fixture.tool(kind);
        assert_eq!(Tool::definition(&tool), &definition());
        assert_eq!(
            Tool::parallel_safety(&tool, &invocation),
            ParallelSafety::Sequential
        );
        assert!(!Tool::enabled_after_interruption(&tool));
        assert_eq!(tool.name(), kind.name());
    }
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn duplicate_fields_validate_every_occurrence_before_service_side_effects() {
    let fixture = Fixture::new();
    for (kind, json) in [
        (Kind::SetCheckpoint, r#"{"title":1,"title":"valid"}"#),
        (
            Kind::SetExitReminder,
            r#"{"title":[],"title":"valid","description":"run"}"#,
        ),
        (
            Kind::SetExitReminder,
            r#"{"title":"valid","description":false,"description":"run"}"#,
        ),
        (Kind::ClearExitReminder, r#"{"title":{},"title":"valid"}"#),
        (
            Kind::CompactContext,
            r#"{"target_context_size":20,"target_context_size":"20%"}"#,
        ),
    ] {
        assert!(
            support::text(fixture.invoke(kind, json).await).starts_with("error:"),
            "{kind:?}: {json}"
        );
    }
    assert!(fixture.calls().is_empty());
    let state = fixture.services.0.lock().unwrap();
    assert!(state.checkpoints.is_empty());
    assert!(state.reminders.is_empty());
}

#[tokio::test]
async fn valid_duplicate_strings_keep_the_last_value_like_source_deserialization() {
    let fixture = Fixture::new();
    assert_eq!(
        support::text(
            fixture
                .invoke(Kind::SetCheckpoint, r#"{"title":"first","title":"last"}"#)
                .await
        ),
        "last"
    );
    assert_eq!(
        fixture.calls(),
        [support::Call::Checkpoint(
            "last".into(),
            support::ASSISTANT_SEQUENCE,
            support::CALL_ID.into()
        )]
    );
}

#[test]
fn session_tool_names_are_the_original_canonical_names() {
    assert_eq!(Kind::SetCheckpoint.name(), "set_checkpoint");
    assert_eq!(Kind::SetExitReminder.name(), "set_exit_reminder");
    assert_eq!(Kind::ClearExitReminder.name(), "clear_exit_reminder");
    assert_eq!(Kind::Status.name(), "status");
    assert_eq!(Kind::CompactContext.name(), "compact_context");
}
