use parrot_domain::{contracts::async_ops::Cancellation, protocol::messages::parrot as wire};
use parrot_host::web::router::WebClient;

use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::SlashFailure,
};

use super::{load_command, model_alias::resolve, model_selection::ModelSelection, rpc};

/// The same model picker is consumed by /model, /model-alias, and session clear.
#[derive(Clone)]
pub struct ModelWizard {
    client: WebClient,
}

impl ModelWizard {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }

    pub async fn select(
        &self,
        current: Option<&str>,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, SlashFailure> {
        self.select_inner(current, false, dialog, cancellation)
            .await
    }

    pub async fn select_explicit_effort(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, SlashFailure> {
        self.select_inner(None, true, dialog, cancellation).await
    }

    async fn select_inner(
        &self,
        current: Option<&str>,
        select_effort: bool,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, SlashFailure> {
        let mut client = self.client.clone();
        let listed = load_command(
            dialog,
            "Loading models…",
            rpc(
                client.list_models(wire::ListModelsRequest::new()),
                cancellation,
            ),
            cancellation,
        )
        .await?;
        let mut providers: Vec<DialogOption> = Vec::new();
        for model in &listed.models {
            if !providers
                .iter()
                .any(|option| option.id == model.provider_id)
            {
                providers.push(DialogOption::new(
                    &model.provider_id,
                    &model.provider_id,
                    "Model provider",
                ));
            }
        }
        if providers.is_empty() {
            dialog
                .show_error("no providers are configured", cancellation)
                .await?;
            return Ok(None);
        }
        let Some(provider) = dialog
            .select("Select a provider", &providers, cancellation)
            .await?
        else {
            return Ok(None);
        };
        let provider_models: Vec<_> = listed
            .models
            .iter()
            .filter(|model| model.provider_id == provider.id)
            .collect();
        let options: Vec<_> = provider_models
            .iter()
            .map(|model| DialogOption::new(&model.id, &model.id, &provider.id))
            .collect();
        let Some(model) = dialog
            .select("Select a model", &options, cancellation)
            .await?
        else {
            return Ok(None);
        };
        let matches: Vec<_> = provider_models
            .into_iter()
            .filter(|item| item.id == model.id)
            .collect();
        let selected = match matches.as_slice() {
            [model] => ModelSelection::new((*model).clone(), None),
            [] => {
                return Err(SlashFailure::Command(
                    "Sequence contains no matching element".into(),
                ));
            }
            _ => {
                return Err(SlashFailure::Command(
                    "Sequence contains more than one matching element".into(),
                ));
            }
        };
        if select_effort && !selected.model.variants.is_empty() {
            let options: Vec<_> = selected
                .model
                .variants
                .iter()
                .map(|variant| {
                    DialogOption::new(&variant.name, &variant.name, &variant.reasoning_effort)
                })
                .collect();
            let effort = dialog
                .select("Select model effort", &options, cancellation)
                .await?;
            return Ok(effort
                .and_then(|effort| selected.with_variant(&effort.id))
                .map(|selection| selection.selector()));
        }
        let canonical = match current {
            Some(selector) => Some(resolve(&self.client, selector, cancellation).await?),
            None => None,
        };
        let current = canonical
            .as_deref()
            .and_then(|selector| ModelSelection::resolve(&listed.models, selector));
        let selected = match current.and_then(|selection| selection.variant) {
            Some(variant) => selected
                .with_variant(&variant.name)
                .unwrap_or_else(|| selected.with_first_variant()),
            None => selected.with_first_variant(),
        };
        Ok(Some(selected.selector()))
    }
}
