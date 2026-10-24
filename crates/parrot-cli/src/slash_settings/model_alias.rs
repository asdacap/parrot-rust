use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_host::web::router::WebClient;
use tonic::Code;

use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{CommandContext, SlashCommand, SlashFailure},
};

use super::{load_command, model_wizard::ModelWizard, rpc};

pub async fn resolve(
    client: &WebClient,
    selector: &str,
    cancellation: &dyn Cancellation,
) -> Result<String, SlashFailure> {
    if selector.contains('/') {
        return Ok(selector.to_owned());
    }
    let mut client = client.clone();
    match rpc(
        client.list_model_aliases(wire::ListModelAliasesRequest::new()),
        cancellation,
    )
    .await
    {
        Ok(listed) => Ok(listed
            .aliases
            .into_iter()
            .find(|alias| alias.name == selector)
            .map_or_else(|| selector.to_owned(), |alias| alias.model_string)),
        Err(SlashFailure::Rpc(status)) if status.code() == Code::Unimplemented => {
            Ok(selector.to_owned())
        }
        Err(failure) => Err(failure),
    }
}

pub async fn warnings(
    client: &WebClient,
    cancellation: &dyn Cancellation,
) -> Result<Vec<String>, SlashFailure> {
    let mut client = client.clone();
    match rpc(
        client.list_model_aliases(wire::ListModelAliasesRequest::new()),
        cancellation,
    )
    .await
    {
        Ok(listed) => {
            let mut aliases: Vec<_> = listed
                .aliases
                .into_iter()
                .filter(|alias| alias.model_string.is_empty())
                .collect();
            aliases.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            Ok(aliases
                .into_iter()
                .map(|alias| {
                    format!(
                        "warning: model alias \"{}\" is not configured. Use /model-alias to configure.",
                        alias.name
                    )
                })
                .collect())
        }
        Err(SlashFailure::Rpc(_)) => Ok(Vec::new()),
        Err(failure) => Err(failure),
    }
}

pub struct ModelAliasCommand {
    client: WebClient,
    models: ModelWizard,
}

impl ModelAliasCommand {
    pub fn new(client: WebClient, models: ModelWizard) -> Self {
        Self { client, models }
    }

    async fn run_inner(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<(), SlashFailure> {
        let mut client = self.client.clone();
        let listed = load_command(
            dialog,
            "Loading aliases…",
            rpc(
                client.list_model_aliases(wire::ListModelAliasesRequest::new()),
                cancellation,
            ),
            cancellation,
        )
        .await?;
        let mut aliases = listed.aliases;
        aliases.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
        let mut options = vec![DialogOption::new(
            "/provider-defaults",
            "Use provider defaults",
            "Configure all four model aliases",
        )];
        options.extend(aliases.iter().map(|alias| {
            DialogOption::new(
                &alias.name,
                &alias.name,
                format!(
                    "{}; {}",
                    if alias.model_string.is_empty() {
                        "not configured"
                    } else {
                        &alias.model_string
                    },
                    alias.usage
                ),
            )
        }));
        let Some(selected) = dialog
            .select("Select a model alias", &options, cancellation)
            .await?
        else {
            return Ok(());
        };
        if selected.id == "/provider-defaults" {
            let defaults = load_command(
                dialog,
                "Loading provider defaults…",
                rpc(
                    client.list_provider_model_alias_defaults(
                        wire::ListProviderModelAliasDefaultsRequest::new(),
                    ),
                    cancellation,
                ),
                cancellation,
            )
            .await?;
            if defaults.providers.is_empty() {
                dialog
                    .show_error(
                        "no available providers have model alias defaults",
                        cancellation,
                    )
                    .await?;
                return Ok(());
            }
            let mut providers = defaults.providers;
            providers.sort_by(|left, right| {
                left.provider_id
                    .encode_utf16()
                    .cmp(right.provider_id.encode_utf16())
            });
            let options: Vec<_> = providers
                .iter()
                .map(|provider| DialogOption::new(&provider.provider_id, &provider.provider_id, ""))
                .collect();
            let Some(provider) = dialog
                .select("Select provider defaults", &options, cancellation)
                .await?
            else {
                return Ok(());
            };
            let applied = load_command(
                dialog,
                "Applying provider defaults…",
                rpc(
                    client.apply_provider_model_alias_defaults(
                        wire::ApplyProviderModelAliasDefaultsRequest {
                            provider_id: provider.id.clone(),
                            ..Default::default()
                        },
                    ),
                    cancellation,
                ),
                cancellation,
            )
            .await?;
            let mut aliases = applied.aliases;
            aliases.sort_by(|left, right| left.name.encode_utf16().cmp(right.name.encode_utf16()));
            let mut lines = vec![format!(
                "Model aliases configured from {} defaults:",
                provider.id
            )];
            lines.extend(
                aliases
                    .iter()
                    .map(|alias| format!("  {} = {}", alias.name, alias.model_string)),
            );
            dialog.show(&lines, cancellation).await?;
            return Ok(());
        }
        let Some(target) = self
            .models
            .select_explicit_effort(dialog, cancellation)
            .await?
        else {
            return Ok(());
        };
        rpc(
            client.configure_model_alias(wire::ConfigureModelAliasRequest {
                name: selected.id.clone(),
                model_string: target.clone(),
                ..Default::default()
            }),
            cancellation,
        )
        .await?;
        dialog
            .show(
                &[format!(
                    "Model alias configured: {} = {target}",
                    selected.id
                )],
                cancellation,
            )
            .await?;
        Ok(())
    }
}

impl SlashCommand for ModelAliasCommand {
    fn name(&self) -> &str {
        "/model-alias"
    }

    fn summary(&self) -> &str {
        "Configure a model alias"
    }

    fn run<'a>(
        &'a self,
        _arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            match self.run_inner(context.dialog, cancellation).await {
                Err(SlashFailure::Rpc(status)) => context
                    .dialog
                    .show_error(status.message(), cancellation)
                    .await
                    .map_err(Into::into),
                result => result,
            }
        })
    }
}
