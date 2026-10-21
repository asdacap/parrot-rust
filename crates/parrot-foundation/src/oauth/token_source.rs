use super::{AuthError, CredentialStorage, OAuthAccess, OAuthClient};
use crate::credentials::{CURRENT_VERSION, Credential, CredentialType, OAuthCredential};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult, OwnedWork,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

#[cfg(test)]
#[path = "token_source_tests.rs"]
mod tests;

type RefreshResult = OperationResult<OAuthCredential, AuthError>;

struct Flight {
    completion: watch::Receiver<Option<Arc<RefreshResult>>>,
}

struct Inner {
    store: Arc<dyn CredentialStorage>,
    client: Arc<dyn OAuthClient>,
    name: String,
    flight: tokio::sync::Mutex<Option<Arc<Flight>>>,
}

/// Owns token operations, including refresh work after an observation future is dropped.
/// The session must quiesce admissions before calling `settle` at shutdown.
pub struct OAuthTokenSource {
    inner: Arc<Inner>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl OAuthTokenSource {
    pub fn new(
        store: Arc<dyn CredentialStorage>,
        client: Arc<dyn OAuthClient>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                client,
                name: name.into(),
                flight: tokio::sync::Mutex::new(None),
            }),
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, AuthError>> {
        Box::pin(async move {
            let credential = self.inner.store.get(&self.inner.name, cancellation).await?;
            let Some(credential) = credential else {
                return Ok(false);
            };
            if credential.version != CURRENT_VERSION
                || credential.credential_type != CredentialType::OAuth
                || credential.api_key.is_some()
            {
                return Ok(false);
            }
            Ok(credential.oauth.is_some_and(|oauth| {
                !oauth.access_token.value().is_empty()
                    && !oauth.refresh_token.value().is_empty()
                    && chrono::DateTime::parse_from_rfc3339(&oauth.expires_at).is_ok_and(|date| {
                        date.timestamp() != -62_135_596_800 || date.timestamp_subsec_nanos() != 0
                    })
            }))
        })
    }

    pub fn token(
        &self,
        cancellation: Arc<dyn Cancellation>,
    ) -> BoxFuture<'_, OperationResult<OAuthAccess, AuthError>> {
        let inner = self.inner.clone();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = inner
                .token(cancellation.as_ref())
                .await
                .map(OAuthAccess::from);
            let _ = sender.send(result);
        });
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or(Err(OperationError::Failed(AuthError::Message(
                    "auth: token operation failed",
                ))))
        })
    }
}

impl OwnedWork for OAuthTokenSource {
    type Error = AuthError;

    fn settle(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            let mut failed = false;
            loop {
                let tasks = std::mem::take(
                    &mut *self
                        .tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                if tasks.is_empty() {
                    break;
                }
                for task in tasks {
                    failed |= task.await.is_err();
                }
            }
            if failed {
                Err(AuthError::Message("auth: token operation failed"))
            } else {
                Ok(())
            }
        })
    }
}

impl Inner {
    async fn read_oauth(&self, cancellation: &dyn Cancellation) -> RefreshResult {
        let credential = self.store.get(&self.name, cancellation).await?;
        credential
            .filter(|value| value.credential_type == CredentialType::OAuth)
            .and_then(|value| value.oauth)
            .ok_or(OperationError::Failed(AuthError::Message(
                "auth: credential is not OAuth",
            )))
    }

    fn fresh(&self, credential: &OAuthCredential) -> Result<bool, OperationError<AuthError>> {
        let expiry = chrono::DateTime::parse_from_rfc3339(&credential.expires_at)
            .map(SystemTime::from)
            .map_err(|_| {
                OperationError::Failed(AuthError::Message("auth: invalid credential expiry"))
            })?;
        let boundary = self
            .client
            .now()
            .checked_add(Duration::from_secs(300))
            .ok_or(OperationError::Failed(AuthError::Message(
                "auth: invalid credential expiry",
            )))?;
        Ok(expiry > boundary)
    }

    async fn token(&self, cancellation: &dyn Cancellation) -> RefreshResult {
        let current = self.read_oauth(cancellation).await?;
        if self.fresh(&current)? {
            return Ok(current);
        }
        let mut gate = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(OperationError::Cancelled),
            gate = self.flight.lock() => gate,
        };
        if let Some(flight) = gate.as_ref() {
            let flight = flight.clone();
            drop(gate);
            return await_flight(flight).await;
        }
        let latest = self.read_oauth(cancellation).await?;
        if self.fresh(&latest)? {
            return Ok(latest);
        }
        let (sender, completion) = watch::channel(None);
        let flight = Arc::new(Flight { completion });
        *gate = Some(flight.clone());
        drop(gate);

        let result = async {
            let refreshed = self.client.refresh(&latest, cancellation).await?;
            self.store
                .set(
                    &self.name,
                    &Credential::for_oauth(refreshed.clone()),
                    cancellation,
                )
                .await?;
            Ok(refreshed)
        }
        .await;
        let mut gate = self.flight.lock().await;
        if gate
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &flight))
        {
            *gate = None;
        }
        drop(gate);
        let _ = sender.send(Some(Arc::new(result.clone())));
        result
    }
}

async fn await_flight(flight: Arc<Flight>) -> RefreshResult {
    let mut completion = flight.completion.clone();
    loop {
        if let Some(result) = completion.borrow().as_ref() {
            return result.as_ref().clone();
        }
        if completion.changed().await.is_err() {
            return Err(OperationError::Failed(AuthError::Message(
                "auth: token operation failed",
            )));
        }
    }
}
