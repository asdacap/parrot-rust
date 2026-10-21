use super::{AuthError, CredentialStorage};
use crate::credentials::{CURRENT_VERSION, CredentialType, Secret};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationResult};
use std::sync::Arc;

pub type ApiKeyEnvironment = dyn Fn(&str) -> Option<String> + Send + Sync;

pub struct StoredApiKeySource {
    provider_id: String,
    api_key_env: String,
    store: Arc<dyn CredentialStorage>,
    environment: Arc<ApiKeyEnvironment>,
}
impl StoredApiKeySource {
    pub fn new(
        provider_id: impl Into<String>,
        api_key_env: impl Into<String>,
        store: Arc<dyn CredentialStorage>,
        environment: Arc<ApiKeyEnvironment>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            api_key_env: api_key_env.into(),
            store,
            environment,
        }
    }
    fn environment_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() {
            None
        } else {
            (self.environment)(&self.api_key_env).filter(|value| !value.is_empty())
        }
    }
    pub fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, AuthError>> {
        Box::pin(async move {
            if self.environment_key().is_some() {
                return Ok(true);
            }
            let credential = self.store.get(&self.provider_id, cancellation).await?;
            Ok(credential.is_some_and(|credential| {
                credential.version == CURRENT_VERSION
                    && credential.credential_type == CredentialType::ApiKey
                    && credential.oauth.is_none()
                    && credential
                        .api_key
                        .is_some_and(|api_key| !api_key.key.value().is_empty())
            }))
        })
    }
    pub fn api_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Secret, AuthError>> {
        Box::pin(async move {
            if let Some(key) = self.environment_key() {
                return Ok(Secret::new(key));
            }
            let credential = self.store.get(&self.provider_id, cancellation).await?;
            Ok(credential
                .filter(|credential| credential.credential_type == CredentialType::ApiKey)
                .and_then(|credential| credential.api_key)
                .map_or_else(|| Secret::new(""), |api_key| api_key.key))
        })
    }
}
