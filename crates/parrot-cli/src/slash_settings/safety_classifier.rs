use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use crate::slash_sessions::{CommandEnvironment, SlashCommand, SlashFailure};

pub struct SafetyClassifierEnabledCommand;

impl SlashCommand for SafetyClassifierEnabledCommand {
    fn name(&self) -> &str {
        "/safety-classifier-enabled"
    }

    fn summary(&self) -> &str {
        "Toggle the bash command safety classifier"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let enabled = match arguments.trim() {
                "true" => true,
                "false" => false,
                _ => {
                    environment
                        .dialog
                        .show_error(
                            "Usage: /safety-classifier-enabled [true|false]",
                            cancellation,
                        )
                        .await?;
                    return Ok(());
                }
            };
            let (_, model) = environment
                .session
                .set_safety_classifier_enabled(enabled, cancellation)
                .await?;
            let state = if enabled { "enabled" } else { "disabled" };
            environment
                .dialog
                .show(
                    &[format!(
                        "Safety classifier is {state} (saved; model {model}). While enabled, exec_command commands are classified before they run and rejected commands need your approval."
                    )],
                    cancellation,
                )
                .await?;
            Ok(())
        })
    }
}
