//! Optional external metadata fetch; catalogue data never configures endpoints.
use crate::{
    catalogs::{Model, ModelsDevCatalogueDecoder},
    http_stream::{self, HttpError, HttpTransport},
    sessions::Error,
};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{collections::BTreeMap, sync::Arc};

pub struct ModelsDevInformationProvider {
    transport: Arc<dyn HttpTransport>,
}
impl ModelsDevInformationProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
    pub async fn fetch(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<BTreeMap<String, Vec<Model>>, Error> {
        let result = async {
            let endpoint =
                http_stream::endpoint_url("https://models.dev", "api.json", false, false)?;
            let body = http_stream::get(
                self.transport.as_ref(),
                &endpoint,
                Vec::new(),
                http_stream::MODELS_REFRESH_TIMEOUT,
                16 << 20,
                cancellation,
            )
            .await?;
            ModelsDevCatalogueDecoder::decode(&body)
                .map(|models| models.into_iter().collect())
                .map_err(|error| Error::Provider(error.to_string()))
        }
        .await;
        match result {
            Err(error) if !cancellation.is_cancelled() && unavailable(&error) => {
                Ok(BTreeMap::new())
            }
            result => result,
        }
    }
}
fn unavailable(error: &Error) -> bool {
    matches!(
        error,
        Error::Provider(_)
            | Error::Encode(_)
            | Error::Io(_)
            | Error::Cancelled
            | Error::Cancellation(_)
            | Error::Http(
                HttpError::Response(_)
                    | HttpError::Http(_)
                    | HttpError::ProviderHttp(_)
                    | HttpError::Transport(_)
                    | HttpError::Cancelled
                    | HttpError::RequestCancelled(_)
                    | HttpError::Provider(_)
            )
    )
}
