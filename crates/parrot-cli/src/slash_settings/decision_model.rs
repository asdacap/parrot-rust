use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use crate::slash_sessions::{CommandEnvironment, SlashCommand, SlashFailure};

pub struct DecisionModelCommand;

impl SlashCommand for DecisionModelCommand {
    fn name(&self) -> &str {
        "/decision-model"
    }

    fn summary(&self) -> &str {
        "Select the bash command safety classifier model"
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
        environment: &'a mut CommandEnvironment<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let selected = arguments.trim();
            if selected.is_empty() {
                let (_, model) = environment
                    .session
                    .get_safety_classifier(cancellation)
                    .await?;
                environment
                    .dialog
                    .show(
                        &[format!(
                            "Decision model is {model}. The provider prefix selects the serving decisions provider."
                        )],
                        cancellation,
                    )
                    .await?;
                return Ok(());
            }
            if selected.split('/').count() < 2 {
                environment
                    .dialog
                    .show_error(
                        "The decision model must be provider-qualified, like openrouter/perplexity/pplx-decider-v1.2",
                        cancellation,
                    )
                    .await?;
                return Ok(());
            }
            let (_, model) = environment
                .session
                .set_decision_model(selected, cancellation)
                .await?;
            environment
                .dialog
                .show(
                    &[format!(
                        "Decision model is now {model} (saved). The classifier uses it for later commands."
                    )],
                    cancellation,
                )
                .await?;
            Ok(())
        })
    }
}
