use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::collections::BTreeMap;

struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn progress_messages_render_from_the_production_catalog() {
    let (_, templates) = PromptTemplateCatalog::from_content(
        "",
        &BTreeMap::from([("HOME".into(), "/synthetic-progress-config".into())]),
    )
    .unwrap();
    let reminder = templates
        .render(
            "agent-session.progress-reminder",
            &[
                ("steps".into(), "5".into()),
                ("total_steps".into(), "5".into()),
                ("current_step".into(), "Running tests".into()),
            ],
            &NeverCancelled,
        )
        .unwrap();
    assert!(reminder.contains("progress"));
    assert!(reminder.contains("close"));
    assert!(reminder.contains("5"));
    assert!(reminder.contains("Running tests"));
    assert!(
        !templates
            .render("progress-tool.closed", &[], &NeverCancelled)
            .unwrap()
            .is_empty()
    );
    assert!(
        !templates
            .render("progress-tool.already-closed", &[], &NeverCancelled)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn progress_message_overrides_drive_the_actual_rendered_result() {
    let (_, templates) = PromptTemplateCatalog::from_content(
        "prompt_templates:\n  agent-session.progress-reminder:\n    template: 'CUSTOM {steps}/{total_steps}: {current_step}'\n  progress-tool.updated:\n    template: 'UPDATED {steps}/{total_steps}'\n",
        &BTreeMap::from([("HOME".into(), "/synthetic-progress-config".into())]),
    ).unwrap();
    assert_eq!(
        templates
            .render(
                "agent-session.progress-reminder",
                &[
                    ("steps".into(), "2".into()),
                    ("total_steps".into(), "5".into()),
                    ("current_step".into(), "Testing".into()),
                ],
                &NeverCancelled,
            )
            .unwrap(),
        "CUSTOM 2/5: Testing"
    );
    assert_eq!(
        templates
            .render(
                "progress-tool.updated",
                &[
                    ("steps".into(), "2".into()),
                    ("total_steps".into(), "5".into())
                ],
                &NeverCancelled,
            )
            .unwrap(),
        "UPDATED 2/5"
    );
}
