//! Provider identity, refreshed catalogues and complete canonical selectors.
use crate::{
    catalogs::{Model, ModelVariant},
    http_stream::HttpError,
    responses::ResponsesError,
    sessions::{Error, Provider, Reasoning},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use std::{
    collections::BTreeMap,
    fmt,
    future::poll_fn,
    sync::{Arc, RwLock},
    task::Poll,
};

#[derive(Clone)]
pub struct ProviderModel {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub variant: Option<ModelVariant>,
}
impl ProviderModel {
    pub fn selector(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}/{}", self.provider.id(), self.model.id, variant.name),
            None => format!("{}/{}", self.provider.id(), self.model.id),
        }
    }

    pub fn reasoning(&self) -> Option<Reasoning> {
        self.variant.as_ref().map(|variant| Reasoning {
            effort: variant.reasoning_effort.clone(),
            summary: "auto".into(),
        })
    }
}
impl fmt::Debug for ProviderModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderModel")
            .field("provider", &self.provider.id())
            .field("model", &self.model)
            .field("variant", &self.variant)
            .finish()
    }
}

pub struct ProviderRegistry {
    by_id: BTreeMap<String, Arc<dyn Provider>>,
    ordered: Vec<Arc<dyn Provider>>,
    catalogues: RwLock<BTreeMap<String, Vec<Model>>>,
}
impl ProviderRegistry {
    pub fn new(
        providers: Vec<Arc<dyn Provider>>,
        catalogues: BTreeMap<String, Vec<Model>>,
    ) -> Result<Self, Error> {
        let mut by_id = BTreeMap::new();
        for provider in &providers {
            if provider.id().is_empty() || by_id.contains_key(provider.id()) {
                return Err(Error::Provider(format!(
                    "provider: duplicate or unnamed provider \"{}\"",
                    provider.id()
                )));
            }
            by_id.insert(provider.id().to_owned(), provider.clone());
        }
        let mut ordered = providers;
        ordered.sort_by(|left, right| left.id().encode_utf16().cmp(right.id().encode_utf16()));
        Ok(Self {
            by_id,
            ordered,
            catalogues: RwLock::new(catalogues),
        })
    }

    pub fn list(&self) -> Vec<Arc<dyn Provider>> {
        self.ordered.clone()
    }

    pub fn all_models(&self) -> Vec<ProviderModel> {
        self.ordered
            .iter()
            .flat_map(|provider| {
                self.models(provider.id())
                    .into_iter()
                    .map(|model| ProviderModel {
                        provider: provider.clone(),
                        model,
                        variant: None,
                    })
            })
            .collect()
    }

    pub fn models(&self, provider_id: &str) -> Vec<Model> {
        self.catalogues
            .read()
            .expect("provider catalogues lock")
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn available_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<ProviderModel>, Error>> {
        Box::pin(async move {
            let mut pending: Vec<_> = self
                .ordered
                .iter()
                .map(|provider| Some(refresh(provider.as_ref(), cancellation)))
                .collect();
            let mut completed: Vec<_> = (0..pending.len()).map(|_| None).collect();
            poll_fn(|context| {
                for (future, result) in pending.iter_mut().zip(&mut completed) {
                    if let Some(active) = future {
                        if let Poll::Ready(value) = active.as_mut().poll(context) {
                            *result = Some(value);
                            *future = None;
                        }
                    }
                }
                if pending.iter().all(Option::is_none) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            // Like Task.WhenAll, settle every refresh before exposing a failure,
            // and do not publish any catalogue updates if the joined refresh fails.
            let refreshes: Result<Vec<_>, _> = completed
                .into_iter()
                .map(|result| result.expect("completed provider refresh"))
                .collect();
            let mut available = Vec::new();
            for (provider, (has_credential, models)) in self.ordered.iter().zip(refreshes?) {
                if !has_credential {
                    continue;
                }
                if let Some(models) = models {
                    self.catalogues
                        .write()
                        .expect("provider catalogues lock")
                        .insert(provider.id().to_owned(), models);
                }
                available.extend(self.models(provider.id()).into_iter().map(|model| {
                    ProviderModel {
                        provider: provider.clone(),
                        model,
                        variant: None,
                    }
                }));
            }
            Ok(available)
        })
    }

    pub fn resolve_canonical(&self, selector: &str) -> Result<ProviderModel, Error> {
        if selector.is_empty() {
            return Err(Error::Provider(
                "provider: a canonical model selector is required".into(),
            ));
        }
        let (provider_id, model_selector) = selector
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .filter(|_| !selector.contains("//"))
            .ok_or_else(|| {
                Error::Provider(format!("provider: malformed model selector \"{selector}\""))
            })?;
        let provider = self.by_id.get(provider_id).cloned().ok_or_else(|| {
            Error::Provider(format!("provider: unknown provider \"{provider_id}\""))
        })?;
        let models = self.models(provider.id());
        let selected = |model, variant| ProviderModel {
            provider: provider.clone(),
            model,
            variant,
        };
        if let Some(exact) = models.iter().find(|model| model.id == model_selector) {
            return Ok(selected(exact.clone(), None));
        }
        let mut matches = models.iter().flat_map(|model| {
            model
                .capabilities
                .variants
                .iter()
                .filter(move |variant| model_selector == format!("{}/{}", model.id, variant.name))
                .map(move |variant| (model, variant))
        });
        if let Some((model, variant)) = matches.next() {
            if matches.next().is_some() {
                return Err(Error::Provider(format!(
                    "provider: ambiguous model selector \"{selector}\""
                )));
            }
            return Ok(selected(model.clone(), Some(variant.clone())));
        }
        if let Some((model_id, variant_name)) = model_selector.rsplit_once('/') {
            if !model_id.is_empty() {
                let mut variants = Vec::new();
                for variant in models.iter().flat_map(|model| &model.capabilities.variants) {
                    if variant.name == variant_name && !variants.contains(variant) {
                        variants.push(variant.clone());
                        if variants.len() == 2 {
                            break;
                        }
                    }
                }
                if variants.len() == 1 {
                    return Ok(selected(
                        Model::new(model_id, provider.id()),
                        variants.pop(),
                    ));
                }
            }
        }
        Ok(selected(Model::new(model_selector, provider.id()), None))
    }

    pub fn resolve_default_canonical(&self) -> Result<ProviderModel, Error> {
        let provider = self
            .ordered
            .first()
            .ok_or_else(|| Error::Provider("provider: no providers configured".into()))?;
        let model = self
            .models(provider.id())
            .into_iter()
            .min_by(|left, right| left.id.encode_utf16().cmp(right.id.encode_utf16()))
            .ok_or_else(|| {
                Error::Provider(format!("provider: \"{}\" serves no models", provider.id()))
            })?;
        Ok(ProviderModel {
            provider: provider.clone(),
            model,
            variant: None,
        })
    }
}

type Refresh = (bool, Option<Vec<Model>>);

fn refresh<'a>(
    provider: &'a dyn Provider,
    cancellation: &'a dyn Cancellation,
) -> BoxFuture<'a, Result<Refresh, Error>> {
    Box::pin(async move {
        match provider.has_credential(cancellation).await {
            Ok(true) => {}
            Ok(false) => return Ok((false, None)),
            Err(error) if is_refresh_failure(&error) => return Ok((false, None)),
            Err(error) => return Err(error),
        }
        match provider.list_models(cancellation).await {
            Ok(models) => Ok((true, Some(models))),
            Err(error) if is_refresh_failure(&error) => Ok((true, None)),
            Err(error) => Err(error),
        }
    })
}

fn is_refresh_failure(error: &Error) -> bool {
    match error {
        Error::Provider(_)
        | Error::ProviderWithCause { .. }
        | Error::HeaderTimeout(_)
        | Error::Wire(_)
        | Error::Io(_) => true,
        Error::Authentication(error) => matches!(
            error,
            AuthError::Message(_)
                | AuthError::Http { .. }
                | AuthError::Credential(_)
                | AuthError::BrowserLaunch(_)
                | AuthError::Transport
                | AuthError::TransportWithMessage(_)
                | AuthError::Io
                | AuthError::IoWithMessage(_)
        ),
        Error::Http(error) => matches!(
            error,
            HttpError::Http(_)
                | HttpError::ProviderHttp(_)
                | HttpError::Provider(_)
                | HttpError::Response(_)
                | HttpError::Wire(_)
                | HttpError::RequestTooLarge(_)
                | HttpError::HeaderTimeout(_)
                | HttpError::Transport(_)
        ),
        Error::ChatProvider(_) => true,
        Error::Response(error) => matches!(
            error,
            ResponsesError::WireProtocol(_) | ResponsesError::ProviderResponse { .. }
        ),
        _ => false,
    }
}
