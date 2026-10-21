use super::{Endpoint, HttpError, has_authority};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Clients {
    client: reqwest::Client,
    strict: reqwest::Client,
}
struct ClientState {
    clients: Mutex<Option<Clients>>,
    authority: Option<String>,
    disposed: AtomicBool,
    closed: tokio::sync::Notify,
}

/// A shared owning client handle. A relaxed TLS client remains authority-scoped
/// even when a caller reuses its handle for a different URI.
#[derive(Clone)]
pub struct ProviderHttpClient(Arc<ClientState>);
impl ProviderHttpClient {
    pub fn standard(client: reqwest::Client) -> Self {
        Self::with_clients(client.clone(), client, None)
    }
    fn with_clients(
        client: reqwest::Client,
        strict: reqwest::Client,
        authority: Option<String>,
    ) -> Self {
        Self(Arc::new(ClientState {
            clients: Mutex::new(Some(Clients { client, strict })),
            authority,
            disposed: AtomicBool::new(false),
            closed: tokio::sync::Notify::new(),
        }))
    }
    pub fn same_client(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn is_disposed(&self) -> bool {
        self.0.disposed.load(Ordering::Acquire)
    }
    pub fn dispose(&self) {
        self.0.disposed.store(true, Ordering::Release);
        self.0.clients.lock().unwrap().take();
        self.0.closed.notify_waiters();
    }
    pub(crate) async fn closed(&self) {
        let notified = self.0.closed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_disposed() {
            notified.await;
        }
    }
    pub fn allows_invalid_tls_for(&self, endpoint: impl Into<Endpoint>) -> bool {
        self.0
            .authority
            .as_deref()
            .is_some_and(|authority| has_authority(endpoint, authority))
    }
    pub(crate) fn client_for(&self, endpoint: &Endpoint) -> Result<reqwest::Client, HttpError> {
        let clients = self.0.clients.lock().unwrap();
        let Some(clients) = clients.as_ref() else {
            return Err(HttpError::Disposed);
        };
        Ok(
            if self.0.authority.is_some() && !self.allows_invalid_tls_for(endpoint) {
                clients.strict.clone()
            } else {
                clients.client.clone()
            },
        )
    }
}

/// Caches one non-redirecting relaxed client per normalized HTTPS authority.
/// The caller owns the standard client; disposing this catalog never closes it.
pub struct ProviderHttpClientCatalog {
    standard: ProviderHttpClient,
    insecure: HashMap<String, ProviderHttpClient>,
}
impl ProviderHttpClientCatalog {
    pub fn new(standard: ProviderHttpClient) -> Self {
        Self {
            standard,
            insecure: HashMap::new(),
        }
    }
    pub fn insecure_client_count(&self) -> usize {
        self.insecure.len()
    }
    pub fn resolve(
        &mut self,
        base_url: &str,
        allow_invalid_tls_certificate: bool,
    ) -> Result<ProviderHttpClient, HttpError> {
        if !allow_invalid_tls_certificate {
            return Ok(self.standard.clone());
        }
        let endpoint = match Endpoint::parse(base_url) {
            Ok(endpoint)
                if endpoint.as_url().scheme() == "https"
                    && endpoint
                        .as_url()
                        .host_str()
                        .is_some_and(|host| !host.is_empty()) =>
            {
                endpoint
            }
            _ => return Ok(self.standard.clone()),
        };
        let authority = endpoint.authority().to_owned();
        let key = authority.to_ascii_lowercase();
        if let Some(client) = self.insecure.get(&key) {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        let strict = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        let scoped = ProviderHttpClient::with_clients(client, strict, Some(authority));
        self.insecure.insert(key, scoped.clone());
        Ok(scoped)
    }
    pub fn dispose(&mut self) {
        for client in self.insecure.values() {
            client.dispose();
        }
        self.insecure.clear();
    }
}
impl Drop for ProviderHttpClientCatalog {
    fn drop(&mut self) {
        self.dispose();
    }
}
