use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::config::Configuration;
use parrot_provider::{
    catalogs::{Model, ModelMetadataFields},
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    image_generation::{ImageHttpRequest, ImageTransport, bucket::ImageBucket},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::{
        auth::CredentialSources,
        builder::{
            BuildServices, HttpClients, ImageBucketFactory, ProviderRegistryBuilder,
            SelectedHttpClient,
        },
        factory::{ProviderBuildContext, ProviderImplementation, build_provider},
        models_dev::ModelsDevInformationProvider,
        registry::ProviderRegistry,
        usage::{KimiUsageReporter, OpenCodeGoUsageReporter},
    },
    sessions::{
        Content, Error, Message, Provider, Request,
        decorators::{
            RetryingProvider, SubscriptionUsage, UsageCredits, UsageReportingProvider, UsageWindow,
        },
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

struct Token;
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
const ACTIVE: Token = Token;
struct Fixture {
    directory: PathBuf,
}
impl Fixture {
    fn new(yaml: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t32-factory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("config.yaml"), yaml).unwrap();
        Self { directory }
    }
    fn load(&self) -> Configuration {
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
                self.directory.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect();
        Configuration::load(
            &self.directory.join("config.yaml"),
            &self.directory.join("predefined_config.yaml"),
            &environment,
            |_| Ok(()),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}
struct FixedCredential(String);
impl ApiKeySource for FixedCredential {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.0.is_empty()) })
    }
}
impl OAuthTokenSource for FixedCredential {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: self.0.clone(),
                account_id: "usage-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(!self.0.is_empty()) })
    }
}
struct Credentials(BTreeMap<String, String>);
impl Credentials {
    fn one(id: &str, key: &str) -> Arc<Self> {
        Arc::new(Self([(id.into(), key.into())].into_iter().collect()))
    }
}
impl CredentialSources for Credentials {
    fn api_key(&self, id: &str, _: &str) -> Arc<dyn ApiKeySource> {
        Arc::new(FixedCredential(self.0.get(id).cloned().unwrap_or_default()))
    }
    fn oauth(&self, id: &str) -> Arc<dyn OAuthTokenSource> {
        Arc::new(FixedCredential(self.0.get(id).cloned().unwrap_or_default()))
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
        Box::pin(async { panic!("direct HTTP call must not connect a socket") })
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
            panic!("image operations belong to the separately tested T34 capability")
        })
    }
}
struct NoBucket;
impl ImageBucketFactory for NoBucket {
    fn build(
        &self,
        _: &parrot_foundation::config::runtime::ImageUploadConfig,
        _: Arc<dyn ImageTransport>,
    ) -> Result<Arc<dyn ImageBucket>, Error> {
        panic!("these fixtures do not configure image upload")
    }
}
struct Body {
    bytes: Vec<u8>,
    offset: usize,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Unavailable,
    ModelsDevId,
    LiveModels,
    ExternalMembership,
    OpenRouter,
    ChatGptModels,
    ChatGptCall,
}
struct Transport {
    mode: Mode,
    usage: Option<String>,
    requests: Mutex<Vec<HttpRequest>>,
}
impl Transport {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            usage: None,
            requests: Mutex::new(Vec::new()),
        })
    }
    fn usage(body: &str) -> Arc<Self> {
        Arc::new(Self {
            mode: Mode::Unavailable,
            usage: Some(body.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn post_bodies(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == HttpMethod::Post)
            .map(|r| String::from_utf8(r.body.clone()).unwrap())
            .collect()
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let uri = request.endpoint.as_url().clone();
            let method = request.method;
            self.requests.lock().unwrap().push(request);
            let (status, body) = if let Some(body) = &self.usage {
                (200, body.clone())
            } else {
                match self.mode {
                Mode::Unavailable => (503, "temporarily unavailable".into()),
                Mode::ModelsDevId if uri.as_str() == "https://models.dev/api.json" => (200, r#"{"opencode":{"id":"opencode","models":{"zen":{"id":"zen-only"}}}}"#.into()),
                Mode::ModelsDevId => (503, "temporarily unavailable".into()),
                Mode::LiveModels => (200, r#"{"data":[{"id":"live-model","context_window":321}]}"#.into()),
                Mode::ExternalMembership if uri.as_str() == "https://models.dev/api.json" => (200, r#"{"first":{"id":"first","models":{"external":{"id":"external-first"}}},"second":{"id":"second","models":{"external":{"id":"external-second"}}},"ghost":{"id":"ghost","models":{"external":{"id":"external-ghost"}}}}"#.into()),
                Mode::ExternalMembership => match uri.host_str().unwrap_or_default() { "first.test" => (200, r#"{"data":[{"id":"live-first"}]}"#.into()), "second.test" => (200, r#"{"data":[{"id":"live-second"}]}"#.into()), _ => (503, String::new()) },
                Mode::OpenRouter if method == HttpMethod::Get => (200, r#"{"data":[{"id":"vendor/model"}]}"#.into()),
                Mode::OpenRouter => (200, "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n".into()),
                Mode::ChatGptModels => (200, r#"{"models":[{"slug":"live","display_name":"Live","context_window":321,"visibility":"list"}]}"#.into()),
                Mode::ChatGptCall => (200, "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n".into()),
            }
            };
            Ok(HttpResponse {
                status,
                reason: "Service Unavailable".into(),
                headers: Vec::new(),
                body: Box::new(Body {
                    bytes: body.into_bytes(),
                    offset: 0,
                }),
            })
        })
    }
}
struct Clients {
    transport: Arc<Transport>,
    resolutions: Mutex<Vec<(String, bool)>>,
}
impl HttpClients for Clients {
    fn resolve(&self, base: &str, invalid: bool) -> Result<SelectedHttpClient, Error> {
        self.resolutions
            .lock()
            .unwrap()
            .push((base.into(), invalid));
        Ok(SelectedHttpClient {
            streaming: self.transport.clone(),
            images: Arc::new(NoImages),
            connector: Arc::new(NoSocket),
        })
    }
}
async fn build(
    yaml: &str,
    credentials: Arc<Credentials>,
    transport: Arc<Transport>,
) -> (ProviderRegistry, Arc<Clients>) {
    let fixture = Fixture::new(yaml);
    let clients = Arc::new(Clients {
        transport: transport.clone(),
        resolutions: Mutex::new(Vec::new()),
    });
    let registry = ProviderRegistryBuilder::new(
        fixture.load(),
        BuildServices {
            clients: clients.clone(),
            credentials,
            models_dev: ModelsDevInformationProvider::new(transport),
            image_buckets: Arc::new(NoBucket),
        },
    )
    .build(&ACTIVE)
    .await
    .unwrap();
    (registry, clients)
}
async fn drain(provider: &dyn Provider, request: Request) -> Result<(), Error> {
    let mut stream = provider.call(request)?;
    while stream.next(&ACTIVE).await?.is_some() {}
    stream.close().await
}
fn request(model: &str, maximum: i32) -> Request {
    Request {
        model: model.into(),
        messages: vec![Message::user("hello")],
        max_output_tokens: maximum,
        ..Request::default()
    }
}
fn configured_provider(
    id: &str,
    yaml: &str,
    credentials: &Credentials,
    transport: Arc<Transport>,
) -> Arc<dyn Provider> {
    let fixture = Fixture::new(yaml);
    let configuration = fixture.load();
    build_provider(ProviderBuildContext {
        id,
        config: &configuration.models.providers[id],
        credentials,
        transport,
        images: Arc::new(NoImages),
        connector: Arc::new(NoSocket),
        external_models: &[],
        maximum_request_bytes: 64 << 20,
    })
    .unwrap()
    .provider
}

#[tokio::test]
async fn build_activates_predefined_openai_with_a_seeded_model_catalogue() {
    let transport = Transport::new(Mode::Unavailable);
    let (registry, _) = build(
        "",
        Credentials::one("openai", "placeholder"),
        transport.clone(),
    )
    .await;
    assert!(registry.list().iter().any(|p| p.id() == "openai"));
    assert!(registry.models("openai").iter().any(|m| m.id == "gpt-5.4"));
    assert!(!transport.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn build_reads_the_models_dev_catalogue_named_by_models_dev_id() {
    let (registry, _) = build(
        "",
        Credentials::one("opencode-zen", "placeholder"),
        Transport::new(Mode::ModelsDevId),
    )
    .await;
    assert!(
        registry
            .models("opencode-zen")
            .iter()
            .any(|m| m.id == "zen-only")
    );
}
#[tokio::test]
async fn build_refreshes_catalogues_without_validating_the_configured_model() {
    let transport = Transport::new(Mode::LiveModels);
    let (registry, _) = build("model: configured/not-listed\nproviders:\n  configured:\n    base_url: https://example.test/v1\n", Credentials::one("configured", "key"), transport.clone()).await;
    let selected = registry.resolve_canonical("configured/not-listed").unwrap();
    assert_eq!(registry.models("configured").len(), 1);
    assert_eq!(registry.models("configured")[0].id, "live-model");
    assert_eq!(selected.model.id, "not-listed");
    assert!(!transport.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn build_seeds_custom_provider_from_configured_model_defaults() {
    let (registry, _) = build("providers:\n  custom:\n    base_url: https://example.test/v1\n    model_defaults:\n      seed:\n        name: Seed Name\n        context: 123\n        variants:\n          high:\n            reasoning_effort: high\n          low:\n            reasoning_effort: low\n", Arc::new(Credentials(BTreeMap::new())), Transport::new(Mode::LiveModels)).await;
    let models = registry.models("custom");
    assert_eq!(models.len(), 1);
    let seed = &models[0];
    assert_eq!(seed.id, "seed");
    assert_eq!(seed.name, "Seed Name");
    assert_eq!(seed.context_window, 123);
    assert_eq!(
        seed.capabilities
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "high,low"
    );
}
#[tokio::test]
async fn chatgpt_refresh_keeps_declarations_drops_unserved_defaults_and_uses_served_default_metadata()
 {
    let mut options = ChatGptOptions::new(Arc::new(FixedCredential("token".into())));
    let mut declared = Model::new("declared", "chatgpt");
    declared.name = "Declared".into();
    let mut default = Model::new("default", "chatgpt");
    default.name = "Default".into();
    let mut live = Model::new("live", "chatgpt");
    live.input_price = 0.001;
    live.fields = ModelMetadataFields::INPUT_PRICE;
    options.models = vec![declared];
    options.model_defaults = vec![default, live];
    let provider = ChatGptProvider::new_with_images(
        options,
        Transport::new(Mode::ChatGptModels),
        Arc::new(NoImages),
        Arc::new(NoSocket),
    )
    .unwrap();
    assert_eq!(
        provider
            .seed_models()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "declared,default,live"
    );
    let refreshed = provider.list_models(&ACTIVE).await.unwrap();
    assert_eq!(
        refreshed
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "declared,live"
    );
    assert_eq!(
        refreshed
            .iter()
            .find(|m| m.id == "live")
            .unwrap()
            .input_price,
        0.001
    );
}
#[tokio::test]
async fn chatgpt_call_omits_max_output_tokens() {
    let transport = Transport::new(Mode::ChatGptCall);
    let provider = configured_provider(
        "chatgpt",
        "",
        &Credentials(
            [(String::from("chatgpt"), String::from("token"))]
                .into_iter()
                .collect(),
        ),
        transport.clone(),
    );
    drain(provider.as_ref(), request("gpt-5.6-sol", 4096))
        .await
        .unwrap();
    let bodies = transport.post_bodies();
    assert_eq!(bodies.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert!(body.get("max_output_tokens").is_none());
}
async fn openrouter_preferences(maximum: usize) {
    let transport = Transport::new(Mode::OpenRouter);
    let yaml = format!(
        "request_limits:\n  provider_request_bytes: {maximum}\nproviders:\n  kimi-api:\n    api_key_env: ''\n  kimi-code:\n    api_key_env: ''\n  opencode-go:\n    api_key_env: ''\n"
    );
    let (registry, _) = build(
        &yaml,
        Credentials::one("openrouter", "key"),
        transport.clone(),
    )
    .await;
    let provider = registry
        .list()
        .into_iter()
        .find(|p| p.id() == "openrouter")
        .unwrap();
    let result = drain(provider.as_ref(), request("vendor/model", 0)).await;
    if maximum == 1 {
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("exceeds 1 bytes")
        );
        assert!(matches!(
            result,
            Err(Error::Http(HttpError::RequestTooLarge(1)))
        ));
        assert!(transport.post_bodies().is_empty());
        return;
    }
    result.unwrap();
    let bodies = transport.post_bodies();
    assert_eq!(bodies.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let preferences = body["provider"].as_object().unwrap();
    assert_eq!(preferences.len(), 4);
    assert_eq!(preferences["allow_fallbacks"], true);
    assert_eq!(preferences["require_parameters"], true);
    assert_eq!(preferences["data_collection"], "deny");
    assert_eq!(preferences["zdr"], true);
    assert_eq!(body["include_router_metadata"], true);
}
#[tokio::test]
async fn openrouter_predefined_preferences_reach_the_request_body_limit_1() {
    openrouter_preferences(1).await;
}
#[tokio::test]
async fn openrouter_predefined_preferences_reach_the_request_body_limit_67108864() {
    openrouter_preferences(67108864).await;
}

#[tokio::test]
async fn build_fetches_once_and_keeps_matching_external_membership_after_live_refresh() {
    let transport = Transport::new(Mode::ExternalMembership);
    let credentials = Arc::new(Credentials(
        [
            (String::from("first"), String::from("first-key")),
            (String::from("second"), String::from("second-key")),
        ]
        .into_iter()
        .collect(),
    ));
    let (registry, clients) = build("providers:\n  first:\n    base_url: https://first.test/v1\n    headers:\n      X-Provider: first-secret\n  second:\n    base_url: https://second.test/v1\n", credentials, transport.clone()).await;
    registry.available_models(&ACTIVE).await.unwrap();
    let requests = transport.requests.lock().unwrap();
    let metadata: Vec<_> = requests
        .iter()
        .filter(|r| r.endpoint.as_url().as_str() == "https://models.dev/api.json")
        .collect();
    assert_eq!(metadata.len(), 1);
    assert!(
        !metadata[0]
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("authorization"))
    );
    assert!(
        !metadata[0]
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-provider"))
    );
    assert_eq!(
        registry
            .models("first")
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "external-first,live-first"
    );
    assert_eq!(
        registry
            .models("second")
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "external-second,live-second"
    );
    assert!(!registry.list().iter().any(|p| p.id() == "ghost"));
    assert!(
        !clients
            .resolutions
            .lock()
            .unwrap()
            .iter()
            .any(|(base, _)| base.contains("ghost"))
    );
}

#[tokio::test]
async fn composed_usage_preserves_credentials_requests_decoding_and_retry_capabilities_all_eight_source_tuples()
 {
    for id in ["chatgpt", "kimi", "opencode-go", "compatible"] {
        for has_credential in [true, false] {
            let response = match id {
                "chatgpt" => {
                    r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":25,"reset_at":1735689600,"limit_window_seconds":18000},"secondary_window":{"used_percent":60,"reset_at":1736294400,"limit_window_seconds":604800}},"credits":{"has_credits":true,"balance":"12.50"}}"#
                }
                "kimi" => r#"{"data":{"available_balance":12.50}}"#,
                "opencode-go" => {
                    r#"{"useBalance":true,"usage":{"rolling":{"percent":25,"resetsAt":"2025-01-01T00:00:00Z"},"weekly":{"percent":60,"resetsAt":"2025-01-08T00:00:00Z"}}}"#
                }
                _ => "{}",
            };
            let transport = Transport::usage(response);
            let key = if has_credential { "usage-token" } else { "" };
            let credentials = Credentials([(id.to_owned(), key.to_owned())].into_iter().collect());
            let provider: Arc<dyn Provider> = if id == "chatgpt" {
                let mut options = ChatGptOptions::new(credentials.oauth(id));
                options.models = vec![Model::new("declared-model", id)];
                Arc::new(
                    ChatGptProvider::new_with_images(
                        options,
                        transport.clone(),
                        Arc::new(NoImages),
                        Arc::new(NoSocket),
                    )
                    .unwrap(),
                )
            } else {
                let mut options = CompatibleOptions::new(
                    id,
                    "https://example.test/v1",
                    credentials.api_key(id, ""),
                );
                options.models = vec![Model::new("declared-model", id)];
                let reporter: Option<
                    Arc<dyn parrot_provider::sessions::decorators::UsageReporter>,
                > = match id {
                    "kimi" => Some(Arc::new(
                        KimiUsageReporter::new(&options, transport.clone()).unwrap(),
                    )),
                    "opencode-go" => Some(Arc::new(
                        OpenCodeGoUsageReporter::new(&options, transport.clone()).unwrap(),
                    )),
                    _ => None,
                };
                let compatible: Arc<dyn Provider> = Arc::new(
                    OpenAICompatibleProvider::new_with_images(
                        options,
                        transport.clone(),
                        Arc::new(NoImages),
                        Arc::new(NoSocket),
                    )
                    .unwrap(),
                );
                match reporter {
                    Some(reporter) => Arc::new(UsageReportingProvider::new(compatible, reporter)),
                    None => compatible,
                }
            };
            let retrying = RetryingProvider::new(provider.clone(), 5);
            assert_eq!(provider.id(), id);
            assert_eq!(retrying.id(), provider.id());
            assert_eq!(provider.seed_models().len(), 1);
            assert_eq!(provider.seed_models()[0].id, "declared-model");
            assert_eq!(retrying.seed_models(), provider.seed_models());
            assert!(transport.requests.lock().unwrap().is_empty());
            if id == "compatible" {
                assert!(provider.usage_reporter().is_none());
                assert!(retrying.usage_reporter().is_none());
                continue;
            }
            let reporter = retrying.usage_reporter().unwrap();
            assert!(Arc::ptr_eq(&reporter, &provider.usage_reporter().unwrap()));
            let usage = reporter.usage(&ACTIVE).await;
            if !has_credential {
                assert!(matches!(usage, Err(Error::Provider(_))));
                assert!(transport.requests.lock().unwrap().is_empty());
                continue;
            }
            let primary = UsageWindow {
                used_percent: 25.0,
                reset_at: "2025-01-01T00:00:00+00:00".into(),
                limit_window_seconds: if id == "chatgpt" { 18000 } else { 0 },
            };
            let secondary = UsageWindow {
                used_percent: 60.0,
                reset_at: "2025-01-08T00:00:00+00:00".into(),
                limit_window_seconds: if id == "chatgpt" { 604800 } else { 0 },
            };
            let expected = match id {
                "chatgpt" => SubscriptionUsage {
                    plan_type: "plus".into(),
                    primary_window: Some(primary),
                    secondary_window: Some(secondary),
                    credits: Some(UsageCredits {
                        has_credits: true,
                        balance: "12.50".into(),
                    }),
                },
                "kimi" => SubscriptionUsage {
                    credits: Some(UsageCredits {
                        has_credits: true,
                        balance: "12.50".into(),
                    }),
                    ..SubscriptionUsage::default()
                },
                _ => SubscriptionUsage {
                    primary_window: Some(primary),
                    secondary_window: Some(secondary),
                    credits: Some(UsageCredits {
                        has_credits: true,
                        balance: "75% remaining".into(),
                    }),
                    ..SubscriptionUsage::default()
                },
            };
            assert_eq!(usage.unwrap(), expected, "{id}");
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let recorded = &requests[0];
            assert_eq!(recorded.method, HttpMethod::Get);
            assert_eq!(
                recorded.endpoint.as_url().as_str(),
                match id {
                    "chatgpt" => "https://chatgpt.com/backend-api/wham/usage",
                    "kimi" => "https://example.test/v1/users/me/balance",
                    _ => "https://example.test/v1/usage",
                }
            );
            let headers: BTreeMap<_, _> = recorded
                .headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.as_str()))
                .collect();
            assert_eq!(headers["authorization"], "Bearer usage-token");
            assert!(!headers.contains_key("session-id"));
            assert!(!headers.contains_key("x-opencode-session"));
            if id == "opencode-go" {
                assert_eq!(
                    headers["user-agent"],
                    concat!("parrot/", env!("CARGO_PKG_VERSION"))
                );
            }
            if id == "chatgpt" {
                assert_eq!(headers["chatgpt-account-id"], "usage-account");
                assert_eq!(headers["originator"], "parrot");
                assert_eq!(headers["user-agent"], "parrot");
            }
        }
    }
}

#[test]
fn actual_factory_selects_all_configured_implementations_and_never_uses_catalogue_endpoints() {
    let fixture = Fixture::new(
        "providers:\n  generic:\n    base_url: https://configured.test/v1\n  kimi-api:\n    base_url: https://configured.test/v1\n  kimi-code:\n    base_url: https://configured.test/v1\n  opencode-go:\n    base_url: https://configured.test/v1\n",
    );
    let configuration = fixture.load();
    let credentials = Credentials(BTreeMap::new());
    for (id, expected) in [
        ("openai", ProviderImplementation::OpenAi),
        ("chatgpt", ProviderImplementation::ChatGpt),
        ("openrouter", ProviderImplementation::OpenRouter),
        ("opencode-go", ProviderImplementation::OpenCodeGo),
        ("opencode-zen", ProviderImplementation::Compatible),
        ("kimi-code", ProviderImplementation::KimiCode),
        ("kimi-api", ProviderImplementation::KimiApi),
        ("generic", ProviderImplementation::Compatible),
    ] {
        let transport = Transport::new(Mode::Unavailable);
        let built = build_provider(ProviderBuildContext {
            id,
            config: &configuration.models.providers[id],
            credentials: &credentials,
            transport: transport.clone(),
            images: Arc::new(NoImages),
            connector: Arc::new(NoSocket),
            external_models: &[Model::new("external", "unconfigured")],
            maximum_request_bytes: 64 << 20,
        })
        .unwrap();
        assert_eq!(built.implementation, expected);
        assert_eq!(built.provider.id(), id);
        assert_eq!(
            built.provider.usage_reporter().is_some(),
            matches!(id, "chatgpt" | "opencode-go" | "kimi-api")
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }
    let generic = &configuration.models.providers["generic"];
    let mut anthropic = generic.clone();
    anthropic.provider_type = "anthropic".into();
    anthropic.protocol = "unsupported".into();
    assert_eq!(
        ProviderImplementation::resolve("chatgpt", &anthropic),
        ProviderImplementation::Anthropic
    );
    assert!(ProviderImplementation::Anthropic.can_build(&anthropic));
    anthropic.base_url.clear();
    assert!(!ProviderImplementation::Anthropic.can_build(&anthropic));
    assert!(ProviderImplementation::ChatGpt.can_build(&anthropic));
    let mut invalid = generic.clone();
    invalid.provider_type = "unsupported".into();
    let error = match build_provider(ProviderBuildContext {
        id: "generic",
        config: &invalid,
        credentials: &credentials,
        transport: Transport::new(Mode::Unavailable),
        images: Arc::new(NoImages),
        connector: Arc::new(NoSocket),
        external_models: &[],
        maximum_request_bytes: 64 << 20,
    }) {
        Ok(_) => panic!("unsupported type must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "provider: unsupported provider type \"unsupported\" for \"generic\""
    );
    invalid.provider_type = "compatible".into();
    invalid.protocol = "unsupported".into();
    let error = match build_provider(ProviderBuildContext {
        id: "generic",
        config: &invalid,
        credentials: &credentials,
        transport: Transport::new(Mode::Unavailable),
        images: Arc::new(NoImages),
        connector: Arc::new(NoSocket),
        external_models: &[],
        maximum_request_bytes: 64 << 20,
    }) {
        Ok(_) => panic!("unsupported protocol must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "provider: unsupported protocol \"unsupported\" for \"generic\""
    );
    assert_eq!(
        built_image_cost(&configuration, &credentials, "openai"),
        765
    );
    assert_eq!(
        built_image_cost(&configuration, &credentials, "generic"),
        4096
    );
}
fn built_image_cost(configuration: &Configuration, credentials: &Credentials, id: &str) -> i64 {
    build_provider(ProviderBuildContext {
        id,
        config: &configuration.models.providers[id],
        credentials,
        transport: Transport::new(Mode::Unavailable),
        images: Arc::new(NoImages),
        connector: Arc::new(NoSocket),
        external_models: &[],
        maximum_request_bytes: 64 << 20,
    })
    .unwrap()
    .provider
    .calculate_image_tokens(
        &Model::new("gpt-4o", id),
        &Content::image_link(
            "",
            "https://example.test/image.png",
            "image/png",
            1024,
            1024,
        ),
    )
}
