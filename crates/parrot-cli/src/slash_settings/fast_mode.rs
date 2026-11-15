use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use crate::slash_sessions::{CommandEnvironment, SlashCommand, SlashFailure};

pub struct FastCommand;

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "/fast"
    }

    fn summary(&self) -> &str {
        "Toggle fast mode for ChatGPT requests (increased usage)"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let enabled = match arguments.trim() {
                "" => !environment.session.get_fast_mode(cancellation).await?,
                "on" => true,
                "off" => false,
                _ => {
                    environment
                        .dialog
                        .show_error("Usage: /fast [on|off]", cancellation)
                        .await?;
                    return Ok(());
                }
            };
            let enabled = environment
                .session
                .set_fast_mode(enabled, cancellation)
                .await?;
            let state = if enabled { "on" } else { "off" };
            environment
                .dialog
                .show(
                    &[format!(
                        "Fast mode is {state} (saved; ChatGPT requests only). Fast mode uses more of your subscription allowance."
                    )],
                    cancellation,
                )
                .await?;
            Ok(())
        })
    }
}
