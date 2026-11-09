use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use crate::slash_sessions::{CommandContext, SlashCommand, SlashFailure};

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
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let enabled = match arguments.trim() {
                "" => !context.session.get_fast_mode(cancellation).await?,
                "on" => true,
                "off" => false,
                _ => {
                    context
                        .dialog
                        .show_error("Usage: /fast [on|off]", cancellation)
                        .await?;
                    return Ok(());
                }
            };
            let enabled = context.session.set_fast_mode(enabled, cancellation).await?;
            let state = if enabled { "on" } else { "off" };
            context
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
