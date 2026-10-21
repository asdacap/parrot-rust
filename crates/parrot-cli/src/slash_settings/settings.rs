use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::ListModelsRequest,
    values::context::ContextSize,
};
use parrot_foundation::config::models::validate_preset_name;
use parrot_host::web::router::WebClient;

use super::{ModelSelection, ModelWizard, check_cancelled, load_command, model_alias, rpc};
use crate::{
    dispatch::dialog::DialogOption,
    slash_sessions::{CommandContext, SlashCommand, SlashFailure},
};

pub struct ModelCommand {
    wizard: ModelWizard,
}
impl ModelCommand {
    pub fn new(wizard: ModelWizard) -> Self {
        Self { wizard }
    }
}
impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "/model"
    }
    fn summary(&self) -> &str {
        "Switch the model for this session"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let selected = self
                .wizard
                .select(Some(context.session.model()), context.dialog, cancellation)
                .await?;
            let Some(selected) = selected else {
                return Ok(());
            };
            context.activity.wait_until_idle(cancellation).await?;
            context
                .session
                .select_model(&selected, cancellation)
                .await?;
            context
                .dialog
                .show(&[format!("model is now {selected} (saved)")], cancellation)
                .await?;
            Ok(())
        })
    }
}

pub struct ModelsCommand {
    client: WebClient,
}
impl ModelsCommand {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }
}
impl SlashCommand for ModelsCommand {
    fn name(&self) -> &str {
        "/models"
    }
    fn summary(&self) -> &str {
        "List the models the provider serves"
    }
    fn run<'a>(
        &'a self,
        _: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let mut client = self.client.clone();
            let listed = load_command(
                context.dialog,
                "Loading models…",
                rpc(client.list_models(ListModelsRequest::new()), cancellation),
                cancellation,
            )
            .await?;
            let mut lines = listed
                .models
                .iter()
                .map(|model| format!("{}/{}", model.provider_id, model.id))
                .collect::<Vec<_>>();
            if lines.is_empty() {
                lines.push("no providers are configured".into());
            }
            context.dialog.show(&lines, cancellation).await?;
            Ok(())
        })
    }
}

pub struct EffortCommand {
    client: WebClient,
}
impl EffortCommand {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }
}
impl SlashCommand for EffortCommand {
    fn name(&self) -> &str {
        "/effort"
    }
    fn summary(&self) -> &str {
        "Switch the model effort for this session"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let current_model = context.session.model().to_owned();
            let (selector, listed) = load_command(
                context.dialog,
                "Loading models…",
                async {
                    let selector =
                        model_alias::resolve(&self.client, &current_model, cancellation).await?;
                    let mut client = self.client.clone();
                    let listed =
                        rpc(client.list_models(ListModelsRequest::new()), cancellation).await?;
                    Ok((selector, listed))
                },
                cancellation,
            )
            .await?;
            let Some(current) = ModelSelection::resolve(&listed.models, &selector) else {
                context
                    .dialog
                    .show_error(
                        &format!("unknown selected model: {}", context.session.model()),
                        cancellation,
                    )
                    .await?;
                return Ok(());
            };
            if current.model.variants.is_empty() {
                context
                    .dialog
                    .show_error(
                        &format!("{} exposes no model efforts", current.base_selector()),
                        cancellation,
                    )
                    .await?;
                return Ok(());
            }
            let requested = arguments.trim();
            let effort = if requested.is_empty() {
                let options = current
                    .model
                    .variants
                    .iter()
                    .map(|variant| {
                        DialogOption::new(&variant.name, &variant.name, &variant.reasoning_effort)
                    })
                    .collect::<Vec<_>>();
                context
                    .dialog
                    .select("Select model effort", &options, cancellation)
                    .await?
                    .map(|option| option.id)
            } else {
                Some(requested.to_owned())
            };
            let Some(effort) = effort else {
                return Ok(());
            };
            let Some(replacement) = current.with_variant(&effort) else {
                context
                    .dialog
                    .show_error(&format!("unknown model effort {effort}"), cancellation)
                    .await?;
                return Ok(());
            };
            context.activity.wait_until_idle(cancellation).await?;
            context
                .session
                .select_model(&replacement.selector(), cancellation)
                .await?;
            context
                .dialog
                .show(
                    &[format!(
                        "Model effort selected: {}",
                        replacement
                            .variant
                            .as_ref()
                            .map(|variant| variant.name.as_str())
                            .unwrap_or_default()
                    )],
                    cancellation,
                )
                .await?;
            Ok(())
        })
    }
}

pub struct ModelPresetSetCommand;
impl SlashCommand for ModelPresetSetCommand {
    fn name(&self) -> &str {
        "/model-preset-set"
    }
    fn summary(&self) -> &str {
        "Save the current model selection and aliases as a preset"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            check_cancelled(cancellation)?;
            if validate_preset_name(arguments).is_err() {
                context
                    .dialog
                    .show_error(&preset_usage(self.name()), cancellation)
                    .await?;
                return Ok(());
            }
            match context
                .session
                .set_model_preset(arguments, cancellation)
                .await
            {
                Ok(preset) => {
                    context
                        .dialog
                        .show(
                            &[format!(
                                "Model preset saved: {} = {}",
                                preset.name, preset.model
                            )],
                            cancellation,
                        )
                        .await?
                }
                Err(SlashFailure::Rpc(status)) => {
                    context
                        .dialog
                        .show_error(status.message(), cancellation)
                        .await?
                }
                Err(failure) => return Err(failure),
            }
            Ok(())
        })
    }
}

pub struct ModelPresetSelectCommand;
impl SlashCommand for ModelPresetSelectCommand {
    fn name(&self) -> &str {
        "/model-preset-select"
    }
    fn summary(&self) -> &str {
        "Select a saved model selection preset"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            check_cancelled(cancellation)?;
            let name;
            if arguments.trim().is_empty() {
                context.activity.wait_until_idle(cancellation).await?;
                let presets = load_command(
                    context.dialog,
                    "Loading presets…",
                    context.session.list_model_presets(cancellation),
                    cancellation,
                )
                .await?;
                if presets.is_empty() {
                    context
                        .dialog
                        .show_error("no model presets are saved", cancellation)
                        .await?;
                    return Ok(());
                }
                let options = presets
                    .iter()
                    .map(|preset| DialogOption::new(&preset.name, &preset.name, &preset.model))
                    .collect::<Vec<_>>();
                let Some(selected) = context
                    .dialog
                    .select("Select a model preset", &options, cancellation)
                    .await?
                else {
                    return Ok(());
                };
                name = selected.id;
            } else {
                if validate_preset_name(arguments).is_err() {
                    context
                        .dialog
                        .show_error(&preset_usage(self.name()), cancellation)
                        .await?;
                    return Ok(());
                }
                context.activity.wait_until_idle(cancellation).await?;
                name = arguments.to_owned();
            }
            match context
                .session
                .select_model_preset(&name, cancellation)
                .await
            {
                Ok(preset) => {
                    context
                        .dialog
                        .show(
                            &[format!(
                                "Model preset selected: {} = {}",
                                preset.name, preset.model
                            )],
                            cancellation,
                        )
                        .await?
                }
                Err(SlashFailure::Rpc(status)) => {
                    context
                        .dialog
                        .show_error(status.message(), cancellation)
                        .await?
                }
                Err(failure) => return Err(failure),
            }
            Ok(())
        })
    }
}

pub(crate) fn preset_usage(name: &str) -> String {
    format!("usage: {name} <name>; name must be one token without whitespace or '/'")
}

pub struct SetContextLimitCommand;
impl SlashCommand for SetContextLimitCommand {
    fn name(&self) -> &str {
        "/set-context-limit"
    }
    fn summary(&self) -> &str {
        "Set the automatic context compaction trigger"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            let value = arguments.trim();
            if value.is_empty()
                || value.contains([' ', '\t', '\r', '\n'])
                || ContextSize::parse(value).is_err()
            {
                context
                    .dialog
                    .show_error("usage: /set-context-limit <size>", cancellation)
                    .await?;
                return Ok(());
            }
            match context.session.set_context_limit(value, cancellation).await {
                Ok(response) => {
                    let suffix = if response.alias_override {
                        "; the current model alias override remains effective"
                    } else {
                        ""
                    };
                    context
                        .dialog
                        .show(
                            &[format!("Context compaction limit set to {value}{suffix}")],
                            cancellation,
                        )
                        .await?;
                }
                Err(failure) => {
                    context
                        .dialog
                        .show_error(
                            &format!("failed to set context limit: {}", failure.message()),
                            cancellation,
                        )
                        .await?
                }
            }
            Ok(())
        })
    }
}
