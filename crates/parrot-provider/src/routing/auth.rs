//! Adapters over Foundation-owned credential selection and OAuth lifetime.
use crate::sessions::{
    Error,
    provider::{ApiKeySource, OAuthAccess, OAuthTokenSource},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError, OwnedWork};
use parrot_foundation::oauth::{
    self, ApiKeyEnvironment, AuthError, CredentialStorage, OAuthClient,
};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub trait CredentialSources: Send + Sync {
    fn api_key(&self, id: &str, environment: &str) -> Arc<dyn ApiKeySource>;
    fn oauth(&self, id: &str) -> Arc<dyn OAuthTokenSource>;
}

pub struct FoundationCredentials {
    store: Arc<dyn CredentialStorage>,
    environment: Arc<ApiKeyEnvironment>,
    client: Arc<dyn OAuthClient>,
    token_sources: Mutex<Vec<Arc<oauth::OAuthTokenSource>>>,
}
impl FoundationCredentials {
    pub fn new(
        store: Arc<dyn CredentialStorage>,
        environment: Arc<ApiKeyEnvironment>,
        client: Arc<dyn OAuthClient>,
    ) -> Self {
        Self {
            store,
            environment,
            client,
            token_sources: Mutex::new(Vec::new()),
        }
    }
}
impl CredentialSources for FoundationCredentials {
    fn api_key(&self, id: &str, environment: &str) -> Arc<dyn ApiKeySource> {
        Arc::new(ApiKeyAdapter(oauth::StoredApiKeySource::new(
            id,
            environment,
            self.store.clone(),
            self.environment.clone(),
        )))
    }
    fn oauth(&self, id: &str) -> Arc<dyn OAuthTokenSource> {
        let source = Arc::new(oauth::OAuthTokenSource::new(
            self.store.clone(),
            self.client.clone(),
            id,
        ));
        self.token_sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(source.clone());
        Arc::new(OAuthAdapter(source))
    }
}
impl OwnedWork for FoundationCredentials {
    type Error = AuthError;
    fn settle(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            let sources = self
                .token_sources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut failure = None;
            for source in sources {
                if let Err(error) = source.settle().await {
                    failure.get_or_insert(error);
                }
            }
            failure.map_or(Ok(()), Err)
        })
    }
}

fn auth_error(error: OperationError<AuthError>) -> Error {
    match error {
        OperationError::Cancelled => Error::Cancelled,
        OperationError::Failed(error) => Error::Authentication(error),
    }
}
struct ApiKeyAdapter(oauth::StoredApiKeySource);
impl ApiKeySource for ApiKeyAdapter {
    fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.0
                .api_key(cancellation)
                .await
                .map(|key| key.value().to_owned())
                .map_err(auth_error)
        })
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.0
                .has_credential(cancellation)
                .await
                .map_err(auth_error)
        })
    }
}
struct OAuthAdapter(Arc<oauth::OAuthTokenSource>);
impl OAuthTokenSource for OAuthAdapter {
    fn token<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            let stop = Arc::new(OwnedCancellation::new(cancellation.is_cancelled()));
            let _owner = CancelOnDrop(stop.clone());
            let operation = self.0.token(stop.clone());
            tokio::pin!(operation);
            let result = tokio::select! {
                biased;
                result = &mut operation => result,
                _ = cancellation.cancelled() => { stop.cancel(); operation.await },
            };
            result
                .map(|access| OAuthAccess {
                    access_token: access.access_token.value().to_owned(),
                    account_id: access.account_id.unwrap_or_default(),
                })
                .map_err(auth_error)
        })
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.0
                .has_credential(cancellation)
                .await
                .map_err(auth_error)
        })
    }
}

pub(super) struct OwnedCancellation(watch::Sender<bool>);
impl OwnedCancellation {
    pub(super) fn new(cancelled: bool) -> Self {
        Self(watch::channel(cancelled).0)
    }
    pub(super) fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Cancellation for OwnedCancellation {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        let mut receiver = self.0.subscribe();
        Box::pin(async move {
            loop {
                if *receiver.borrow_and_update() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
struct CancelOnDrop(Arc<OwnedCancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
