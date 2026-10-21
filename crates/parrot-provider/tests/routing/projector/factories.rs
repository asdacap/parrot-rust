//! Input-only collaborator wiring for real configured factories and metadata fetches.
use super::common;
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult, OwnedWork,
};
use parrot_foundation::{
    config::{Configuration, models::ProviderConfig},
    credentials::{Credential, OAuthCredential},
    oauth::{AuthError, CredentialStorage, DeviceAuthorization, OAuthClient},
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    image_generation::{ImageHttpRequest, ImageTransport, bucket::ImageBucket},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::{
        auth::FoundationCredentials,
        builder::{
            BuildServices, HttpClients, ImageBucketFactory, ProviderRegistryBuilder,
            SelectedHttpClient,
        },
        factory::{ProviderBuildContext, ProviderImplementation, build_provider},
        models_dev::ModelsDevInformationProvider,
    },
    sessions::{Error, Event, Message, Request},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending().await
            }
        })
    }
}
const ACTIVE: Token = Token(false);

struct Body {
    bytes: Vec<u8>,
    offset: usize,
    block: bool,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if self.block {
                cancellation.cancelled().await;
                return Err(HttpError::Cancelled);
            }
            let length = output.len().min(self.bytes.len() - self.offset);
            output[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
            self.offset += length;
            Ok(length)
        })
    }
}
#[derive(Clone)]
struct Reply {
    status: u16,
    reason: String,
    body: Vec<u8>,
    block: bool,
    failure: Option<HttpError>,
}
impl Reply {
    fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            reason: "OK".into(),
            body: body.into(),
            block: false,
            failure: None,
        }
    }
}
#[derive(Default)]
struct Records {
    requests: Vec<Value>,
    session_ids: BTreeMap<String, String>,
}
impl Records {
    fn record(
        &mut self,
        method: &str,
        uri: &str,
        headers: Vec<(String, String)>,
        body: Option<&[u8]>,
    ) -> Result<(), HttpError> {
        let mut sorted = BTreeMap::new();
        for (name, mut value) in headers {
            let name = name.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "session-id" | "x-opencode-session" | "x-source-session"
            ) {
                eprintln!("t32-native-session-id {}={value}", name);
                let next = self.session_ids.len() + 1;
                value = self
                    .session_ids
                    .entry(value)
                    .or_insert_with(|| format!("session-{next}"))
                    .clone();
            }
            sorted
                .entry(name)
                .and_modify(|previous: &mut String| {
                    previous.push_str(", ");
                    previous.push_str(&value);
                })
                .or_insert(value);
        }
        let body = body
            .map(|bytes| std::str::from_utf8(bytes).map(str::to_owned))
            .transpose()
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        self.requests
            .push(json!({"method":method,"uri":uri,"headers":sorted,"body_raw":body}));
        Ok(())
    }
    fn snapshot(&self) -> Vec<Value> {
        self.requests.clone()
    }
}
type Handler = dyn Fn(&HttpRequest) -> Reply + Send + Sync;
struct Transport {
    records: Arc<Mutex<Records>>,
    handler: Arc<Handler>,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.records.lock().unwrap().record(
                match request.method {
                    HttpMethod::Get => "GET",
                    HttpMethod::Post => "POST",
                },
                request.endpoint.as_str(),
                request.headers.clone(),
                matches!(request.method, HttpMethod::Post).then_some(request.body.as_slice()),
            )?;
            if cancellation.is_cancelled() {
                return Err(HttpError::RequestCancelled(
                    "The operation was canceled.".into(),
                ));
            }
            let reply = (self.handler)(&request);
            if let Some(error) = reply.failure {
                return Err(error);
            }
            let content_type = if request.method == HttpMethod::Get {
                "application/json; charset=utf-8"
            } else {
                "text/event-stream; charset=utf-8"
            };
            Ok(HttpResponse {
                status: reply.status,
                reason: reply.reason,
                headers: vec![("content-type".into(), content_type.into())],
                body: Box::new(Body {
                    bytes: reply.body,
                    offset: 0,
                    block: reply.block,
                }),
            })
        })
    }
}
struct NoImages;
impl ImageTransport for NoImages {
    fn send<'a>(
        &'a self,
        _: ImageHttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async {
            Err(Error::InvalidOperation(
                "fixture image transport was not supplied".into(),
            ))
        })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async {
            Err(WebSocketError::Transport(
                "fixture WebSocket transport was not supplied".into(),
            ))
        })
    }
}
struct Store {
    credentials: Mutex<BTreeMap<String, Credential>>,
    calls: Mutex<Vec<String>>,
}
impl CredentialStorage for Store {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(name.to_owned());
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            Ok(self.credentials.lock().unwrap().get(name).cloned())
        })
    }
    fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            self.credentials
                .lock()
                .unwrap()
                .insert(name.to_owned(), credential.clone());
            Ok(())
        })
    }
}
struct NoOAuthClient;
impl OAuthClient for NoOAuthClient {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        String::new()
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::Message(
                "fixture OAuth login was not supplied",
            )))
        })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::Message(
                "fixture OAuth login was not supplied",
            )))
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::Message(
                "fixture OAuth login was not supplied",
            )))
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::Message(
                "fixture OAuth refresh was not supplied",
            )))
        })
    }
}
struct Clients(Arc<dyn HttpTransport>);
impl HttpClients for Clients {
    fn resolve(&self, _: &str, _: bool) -> Result<SelectedHttpClient, Error> {
        Ok(SelectedHttpClient {
            streaming: self.0.clone(),
            images: Arc::new(NoImages),
        })
    }
}
struct NoBuckets;
impl ImageBucketFactory for NoBuckets {
    fn build(
        &self,
        _: &parrot_foundation::config::runtime::ImageUploadConfig,
        _: Arc<dyn ImageTransport>,
    ) -> Result<Arc<dyn ImageBucket>, Error> {
        Err(Error::InvalidOperation(
            "fixture image bucket was not supplied".into(),
        ))
    }
}
fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().unwrap_or("")
}
fn integer(value: &Value, name: &str, default: i64) -> i64 {
    value[name].as_i64().unwrap_or(default)
}
fn flag(value: &Value, name: &str, default: bool) -> bool {
    value[name].as_bool().unwrap_or(default)
}

fn transport(
    live: String,
    catalogue: String,
    failure: String,
) -> (Arc<dyn HttpTransport>, Arc<Mutex<Records>>) {
    let records = Arc::new(Mutex::new(Records::default()));
    let handler = move |request: &HttpRequest| {
        let metadata = request.endpoint.as_str() == "https://models.dev/api.json";
        let body = if request.method == HttpMethod::Get {
            if metadata {
                catalogue.clone()
            } else {
                live.clone()
            }
        } else if request.endpoint.as_str().contains("/responses") {
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"source-r\",\"status\":\"completed\",\"output\":[]}}\n\n".into()
        } else if request.endpoint.as_str().contains("/messages") {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"source-m\",\"usage\":{\"input_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into()
        } else {
            "data: {\"choices\":[{\"delta\":{\"content\":\"source-ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()
        };
        let mut reply = Reply::json(body);
        match failure.as_str() {
            "transport" => {
                reply.failure = Some(HttpError::Transport("synthetic unavailable".into()))
            }
            "cancelled" => reply.failure = Some(HttpError::Cancelled),
            "timeout" => reply.block = true,
            "http" => {
                reply.status = 503;
                reply.reason = "Service Unavailable".into();
            }
            "json" if metadata => reply.body = b"not-json".to_vec(),
            "schema" if metadata => reply.body = b"[]".to_vec(),
            "oversized" => reply.body = vec![0; 16_777_217],
            _ => {}
        }
        reply
    };
    (
        Arc::new(Transport {
            records: records.clone(),
            handler: Arc::new(handler),
        }),
        records,
    )
}
fn credentials(values: BTreeMap<String, Credential>) -> (Arc<FoundationCredentials>, Arc<Store>) {
    let store = Arc::new(Store {
        credentials: Mutex::new(values),
        calls: Mutex::new(Vec::new()),
    });
    let credentials = Arc::new(FoundationCredentials::new(
        store.clone(),
        Arc::new(|_| None),
        Arc::new(NoOAuthClient),
    ));
    (credentials, store)
}
fn models(values: &[Model]) -> Vec<Value> {
    values.iter().map(common::model).collect()
}
fn event_kind(value: &Event) -> &'static str {
    match value.base_event() {
        Event::HttpRequestStarted => "HttpRequestStarted",
        Event::HttpResponseHeadersReceived => "HttpResponseHeadersReceived",
        Event::EncryptedReasoningReceived => "EncryptedReasoningReceived",
        Event::TextDelta(_) => "TextDelta",
        Event::ReasoningDelta { .. } => "ReasoningDelta",
        Event::ToolCallDelta(_) => "ToolCallDelta",
        Event::Retry { .. } => "Retry",
        Event::Completed(_) => "Completed",
        Event::WithText { .. } => unreachable!("base_event unwraps text wrappers"),
    }
}

pub async fn run_factory(case: &Value) -> Result<Value, String> {
    let id = text(case, "provider_id");
    let config = ProviderConfig {
        provider_type: text(case, "type").into(),
        protocol: text(case, "protocol").into(),
        base_url: text(case, "base_url").into(),
        api_key_env: text(case, "api_key_env").into(),
        headers: case["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().into()))
            .collect(),
        allow_insecure_localhost: flag(case, "allow_insecure_localhost", false),
        allow_insecure_remote: flag(case, "allow_insecure_remote", false),
        allow_invalid_tls_certificate: flag(case, "allow_invalid_tls_certificate", false),
        disable_websocket: flag(case, "disable_websocket", true),
        image_urls: false,
        stream_idle_timeout_ms: integer(case, "stream_idle_timeout_ms", 300_000) as i32,
        header_timeout_ms: integer(case, "header_timeout_ms", 60_000) as i32,
        header_timeout_max_retries: integer(case, "header_retries", 5) as i32,
        provider_preferences: text(case, "preferences").into(),
        session_header: text(case, "session_header").into(),
        models_dev_id: text(case, "models_dev_id").into(),
        model_defaults: BTreeMap::new(),
        models: BTreeMap::from([("declared-model".into(), Default::default())]),
    };
    let credential = case["credential"].as_str().unwrap_or("factory-key");
    let mut values = BTreeMap::new();
    if !credential.is_empty() {
        values.insert(
            id.into(),
            if id == "chatgpt" {
                Credential::for_oauth(OAuthCredential::create(
                    credential,
                    "synthetic-refresh",
                    "2099-01-01T00:00:00Z",
                    "factory-account",
                ))
            } else {
                Credential::for_api_key(credential)
            },
        );
    }
    let (credentials, store) = credentials(values);
    let (transport, records) = transport(
        case["live_body"]
            .as_str()
            .unwrap_or("{\"data\":[{\"id\":\"live-model\"}]}")
            .into(),
        case["catalogue_body"].as_str().unwrap_or("{}").into(),
        text(case, "failure").into(),
    );
    let external = common::array(case, "external_models")
        .iter()
        .map(|v| common::seed_model(id, v))
        .collect::<Vec<_>>();
    let can_build = ProviderImplementation::resolve(id, &config).can_build(&config);
    let built = match build_provider(ProviderBuildContext {
        id,
        config: &config,
        credentials: credentials.as_ref(),
        transport,
        images: Arc::new(NoImages),
        connector: Arc::new(NoSocket),
        external_models: &external,
        maximum_request_bytes: integer(case, "maximum_request_bytes", 67_108_864) as usize,
    }) {
        Ok(value) => value,
        Err(error) => {
            return Ok(
                json!({"can_build":can_build,"error":common::error(&error),"requests":records.lock().unwrap().snapshot()}),
            );
        }
    };
    let (listed, model_error) = match built.provider.list_models(&ACTIVE).await {
        Ok(value) => (json!(models(&value)), Value::Null),
        Err(error) => (Value::Null, common::error(&error)),
    };
    let mut calls = Vec::new();
    if flag(case, "call", true) {
        let mut events = Vec::new();
        let result: Result<(), Error> = async {
            let session = built.provider.open_session()?;
            let request = Request {
                model: case["model"].as_str().unwrap_or("declared-model").into(),
                messages: vec![Message::user("source-input")],
                max_output_tokens: integer(case, "max_output_tokens", 32) as i32,
                provider_preferences: "{\"request_local\":true}".into(),
                ..Request::default()
            };
            let result = async {
                let mut stream = session.call(request)?;
                let result = loop {
                    match stream.next(&ACTIVE).await {
                        Ok(Some(event)) => events.push(event_kind(&event)),
                        Ok(None) => break Ok(()),
                        Err(error) => break Err(error),
                    }
                };
                let close = stream.close().await;
                result.and(close)
            }
            .await;
            let close = session.close().await;
            result.and(close)
        }
        .await;
        calls.push(json!({"events":events,"error":result.err().as_ref().map(common::error)}));
    }
    credentials.settle().await.map_err(|e| e.to_string())?;
    let provider_type = match built.implementation {
        ProviderImplementation::ChatGpt => "ChatGptProvider",
        ProviderImplementation::Anthropic => "AnthropicProvider",
        ProviderImplementation::KimiApi | ProviderImplementation::OpenCodeGo => {
            "UsageReportingProvider"
        }
        _ => "OpenAICompatibleProvider",
    };
    let gets = store.calls.lock().unwrap().clone();
    Ok(
        json!({"can_build":can_build,"error":null,"provider_id":built.provider.id(),
        "provider_type":provider_type,"reporter_present":built.provider.usage_reporter().is_some(),
        "seeds":models(&built.seed),"models":listed,"model_error":model_error,"calls":calls,
        "requests":records.lock().unwrap().snapshot(),"credential_gets":gets}),
    )
}

pub async fn run_modelsdev(case: &Value) -> Result<Value, String> {
    let (transport, records) = transport(
        "{}".into(),
        case["body_raw"].as_str().unwrap_or("{}").into(),
        text(case, "failure").into(),
    );
    let result = ModelsDevInformationProvider::new(transport)
        .fetch(&Token(flag(case, "cancelled", false)))
        .await;
    let (catalogue, error) = match result {
        Ok(value) => (
            json!(
                value
                    .iter()
                    .map(|(id, values)| (id.clone(), models(values)))
                    .collect::<BTreeMap<_, _>>()
            ),
            Value::Null,
        ),
        Err(error) => (Value::Null, common::error(&error)),
    };
    Ok(json!({"catalogue":catalogue,"error":error,"requests":records.lock().unwrap().snapshot()}))
}

struct Files(std::path::PathBuf);
impl Files {
    fn new(content: &str) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(".tmp");
        std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let directory = loop {
            let path = parent.join(format!(
                "t32-projector-builder-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        };
        let files = Self(directory);
        std::fs::write(files.0.join("config.yaml"), content).map_err(|e| e.to_string())?;
        Ok(files)
    }
    fn load(&self) -> Result<Configuration, String> {
        let environment = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                self.0.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect();
        Configuration::load(
            &self.0.join("config.yaml"),
            &self.0.join("predefined.yaml"),
            &environment,
            |_| Ok(()),
        )
        .map_err(|e| e.to_string())
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn run_builder(case: &Value) -> Result<Value, String> {
    let files = Files::new(text(case, "yaml"))?;
    let configuration = files.load()?;
    let buildable = ProviderRegistryBuilder::buildable_provider_ids(&configuration);
    let values = case["credentials"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, key)| {
            (
                id.clone(),
                Credential::for_api_key(key.as_str().unwrap_or_default()),
            )
        })
        .collect();
    let (credentials, _) = credentials(values);
    let (transport, records) = transport(
        case["live_body"]
            .as_str()
            .unwrap_or("{\"data\":[{\"id\":\"live-model\"}]}")
            .into(),
        case["catalogue_body"].as_str().unwrap_or("{}").into(),
        text(case, "failure").into(),
    );
    let builder = ProviderRegistryBuilder::new(
        configuration,
        BuildServices {
            clients: Arc::new(Clients(transport.clone())),
            credentials: credentials.clone(),
            connector: Arc::new(NoSocket),
            models_dev: ModelsDevInformationProvider::new(transport),
            image_buckets: Arc::new(NoBuckets),
        },
    );
    let result = async {
        let registry = builder.build(&ACTIVE).await?;
        if flag(case, "refresh_again", false) {
            registry.available_models(&ACTIVE).await?;
        }
        Ok::<_, Error>(registry)
    }
    .await;
    credentials.settle().await.map_err(|e| e.to_string())?;
    match result {
        Err(error) => Ok(
            json!({"buildable":buildable,"error":common::error(&error),"requests":records.lock().unwrap().snapshot()}),
        ),
        Ok(registry) => {
            let providers = registry.list();
            let catalogues = providers
                .iter()
                .map(|p| (p.id().to_owned(), models(&registry.models(p.id()))))
                .collect::<BTreeMap<_, _>>();
            let reporters = providers
                .iter()
                .map(|p| (p.id().to_owned(), p.usage_reporter().is_some()))
                .collect::<BTreeMap<_, _>>();
            Ok(
                json!({"buildable":buildable,"error":null,"provider_ids":providers.iter().map(|p|p.id()).collect::<Vec<_>>(),
                "catalogues":catalogues,"reporters":reporters,"requests":records.lock().unwrap().snapshot()}),
            )
        }
    }
}

#[cfg(test)]
mod typed_origin_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn cancelled_request_is_recorded_before_typed_transport_failure() {
        let observation = run_modelsdev(&json!({"cancelled":true})).await.unwrap();
        assert_eq!(observation["requests"].as_array().unwrap().len(), 1);
        assert_eq!(
            observation["error"]["type"],
            "System.Threading.Tasks.TaskCanceledException"
        );
        assert_eq!(
            observation["error"]["message"],
            "The operation was canceled."
        );
    }

    #[tokio::test]
    async fn body_cancellation_remains_generic_cancelled() {
        let mut body = Body {
            bytes: Vec::new(),
            offset: 0,
            block: false,
        };
        let mut buffer = [0];
        assert_eq!(
            body.read(&mut buffer, &Token(true)).await.unwrap_err(),
            HttpError::Cancelled
        );
    }
}
