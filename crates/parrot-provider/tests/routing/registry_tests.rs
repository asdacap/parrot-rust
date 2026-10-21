use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::{Model, ModelVariant},
    http_stream::{HttpError, HttpFailure},
    responses::ResponsesError,
    responses_websocket::WebSocketError,
    routing::registry::ProviderRegistry,
    sessions::{Content, Error, EventStream, Provider, Request, Session},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

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

struct FakeProvider {
    id: String,
    credentials: Mutex<Result<bool, Error>>,
    result: Mutex<Result<Vec<Model>, Error>>,
    credential_calls: AtomicUsize,
    list_calls: AtomicUsize,
    barrier: Option<Arc<Barrier>>,
    release: Option<Arc<Notify>>,
    observe_cancellation: bool,
}
impl FakeProvider {
    fn new(id: &str, has_credential: bool, result: Result<Vec<Model>, Error>) -> Self {
        Self {
            id: id.into(),
            credentials: Mutex::new(Ok(has_credential)),
            result: Mutex::new(result),
            credential_calls: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
            barrier: None,
            release: None,
            observe_cancellation: false,
        }
    }
}
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Err(Error::InvalidOperation("unused call".into()))
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.credential_calls.fetch_add(1, Ordering::SeqCst);
            if self.observe_cancellation && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.credentials.lock().unwrap().clone()
        })
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async move {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            if self.observe_cancellation && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.result.lock().unwrap().clone()
        })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Err(Error::InvalidOperation("unused session".into()))
    }
}
fn provider(id: &str) -> Arc<dyn Provider> {
    Arc::new(FakeProvider::new(id, true, Ok(Vec::new())))
}
fn registry(provider: Arc<dyn Provider>, models: Vec<Model>) -> ProviderRegistry {
    ProviderRegistry::new(
        vec![provider.clone()],
        BTreeMap::from([(provider.id().to_owned(), models)]),
    )
    .unwrap()
}
fn variant_model(id: &str, name: &str, effort: &str) -> Model {
    let mut model = Model::new(id, "p");
    model.capabilities.tools = true;
    model.capabilities.reasoning = true;
    model.capabilities.output = vec!["text".into()];
    model.capabilities.variants = vec![ModelVariant::new(name, effort)];
    model
}
fn selectors(models: Vec<parrot_provider::routing::registry::ProviderModel>) -> Vec<String> {
    models.into_iter().map(|model| model.selector()).collect()
}

#[test]
fn resolve_selects_defaults_and_keeps_the_vendor_prefix() {
    let registry = ProviderRegistry::new(
        vec![provider("openrouter"), provider("opencode-go")],
        BTreeMap::from([
            (
                "openrouter".into(),
                vec![
                    Model::new("openai/gpt-4o", "openrouter"),
                    Model::new("z", "openrouter"),
                ],
            ),
            (
                "opencode-go".into(),
                vec![Model::new("glm-5.2", "opencode-go")],
            ),
        ]),
    )
    .unwrap();
    let explicit_model = registry
        .resolve_canonical("openrouter/openai/gpt-4o")
        .unwrap();
    let default_model = registry
        .resolve_canonical("openrouter/openai/gpt-4o")
        .unwrap();
    let default_provider = registry
        .resolve_canonical("openrouter/openai/gpt-4o")
        .unwrap();
    assert_eq!(explicit_model.model.id, "openai/gpt-4o");
    assert_eq!(default_model.model.id, "openai/gpt-4o");
    assert_eq!(default_provider.provider.id(), "openrouter");
}

#[test]
fn resolve_handles_variants_slash_models_exact_precedence_and_ambiguity() {
    let provider = provider("p");
    let registry = registry(
        provider.clone(),
        vec![
            variant_model("plain", "high", "xhigh"),
            variant_model("vendor/model", "high", "xhigh"),
            Model::new("plain/high", "p"),
            variant_model("ambiguous", "path/high", "one"),
            variant_model("ambiguous/path", "high", "two"),
        ],
    );
    let selected = registry.resolve_canonical("p/vendor/model/high").unwrap();
    let exact = registry.resolve_canonical("p/plain/high").unwrap();
    assert_eq!(selected.model.id, "vendor/model");
    assert_eq!(selected.variant.as_ref().unwrap().name, "high");
    assert_eq!(selected.reasoning().unwrap().effort, "xhigh");
    assert_eq!(selected.reasoning().unwrap().summary, "auto");
    assert_eq!(selected.selector(), "p/vendor/model/high");
    assert_eq!(exact.model.id, "plain/high");
    assert!(exact.variant.is_none());
    assert!(Arc::ptr_eq(&selected.provider, &provider));
    assert_eq!(
        registry
            .resolve_canonical("p/ambiguous/path/high")
            .unwrap_err(),
        Error::Provider("provider: ambiguous model selector \"p/ambiguous/path/high\"".into())
    );
}

#[test]
fn resolve_rejects_unknown_providers_and_malformed_selectors() {
    let registry = registry(provider("p"), vec![Model::new("m", "p")]);
    for selector in ["nope/m", "p", "p/", "/m", "p//m"] {
        let error = registry.resolve_canonical(selector).unwrap_err();
        let expected = if selector == "nope/m" {
            "provider: unknown provider \"nope\"".into()
        } else {
            format!("provider: malformed model selector \"{selector}\"")
        };
        assert_eq!(error, Error::Provider(expected));
    }
    assert_eq!(
        registry.resolve_canonical("").unwrap_err(),
        Error::Provider("provider: a canonical model selector is required".into())
    );
    assert_eq!(
        registry.resolve_canonical("P/m").unwrap_err(),
        Error::Provider("provider: unknown provider \"P\"".into())
    );
}

#[test]
fn resolve_passes_unlisted_models_to_the_provider_and_preserves_known_variant_syntax() {
    let registry = registry(
        provider("p"),
        vec![variant_model("listed", "medium", "medium")],
    );
    let plain = registry.resolve_canonical("p/not-listed").unwrap();
    let variant = registry.resolve_canonical("p/new-model/medium").unwrap();
    assert_eq!(plain.model.id, "not-listed");
    assert!(plain.variant.is_none());
    assert_eq!(variant.model.id, "new-model");
    assert_eq!(variant.variant.unwrap().name, "medium");
    assert_eq!(
        registry
            .resolve_canonical("p/new-model/medium")
            .unwrap()
            .reasoning()
            .unwrap()
            .effort,
        "medium"
    );
}

#[test]
fn duplicate_ids_and_unnamed_providers_are_rejected_at_construction() {
    for ids in [["p", "p"], ["p", ""]] {
        let error = ProviderRegistry::new(ids.into_iter().map(provider).collect(), BTreeMap::new())
            .err()
            .unwrap();
        assert_eq!(
            error,
            Error::Provider(format!(
                "provider: duplicate or unnamed provider \"{}\"",
                ids[1]
            ))
        );
    }
    assert!(ProviderRegistry::new(vec![provider("p"), provider("P")], BTreeMap::new()).is_ok());
    assert!(ProviderRegistry::new(vec![provider(" ")], BTreeMap::new()).is_ok());
}

#[tokio::test]
async fn available_models_skips_uncredentialed_providers_and_keeps_seed_on_refresh_failure() {
    let unavailable = Arc::new(FakeProvider::new(
        "a",
        false,
        Ok(vec![Model::new("not-listed", "a")]),
    ));
    let refreshed = Arc::new(FakeProvider::new(
        "b",
        true,
        Ok(vec![Model::new("fresh", "b")]),
    ));
    let failing = Arc::new(FakeProvider::new(
        "c",
        true,
        Err(Error::Io("offline".into())),
    ));
    let registry = ProviderRegistry::new(
        vec![unavailable.clone(), refreshed.clone(), failing.clone()],
        BTreeMap::from([
            ("a".into(), vec![Model::new("seed-a", "a")]),
            ("b".into(), vec![Model::new("seed-b", "b")]),
            ("c".into(), vec![Model::new("seed-c", "c")]),
        ]),
    )
    .unwrap();
    assert_eq!(
        selectors(registry.available_models(&ACTIVE).await.unwrap()).join(","),
        "b/fresh,c/seed-c"
    );
    assert_eq!(unavailable.list_calls.load(Ordering::SeqCst), 0);
    assert_eq!(refreshed.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(failing.list_calls.load(Ordering::SeqCst), 1);
    let retained_seed = registry.models("a");
    assert_eq!(retained_seed.len(), 1);
    assert_eq!(retained_seed[0].id, "seed-a");
    *unavailable.credentials.lock().unwrap() = Ok(true);
    assert_eq!(
        selectors(registry.available_models(&ACTIVE).await.unwrap()).join(","),
        "a/not-listed,b/fresh,c/seed-c"
    );
    assert_eq!(unavailable.list_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn list_all_models_and_default_use_source_ordinal_order_and_original_provider_references() {
    let a = provider("a");
    let z = provider("z");
    let registry = ProviderRegistry::new(
        vec![z.clone(), a.clone()],
        BTreeMap::from([
            ("a".into(), vec![Model::new("z", "a"), Model::new("a", "a")]),
            ("z".into(), vec![Model::new("m", "z")]),
            ("orphan".into(), vec![Model::new("hidden", "orphan")]),
        ]),
    )
    .unwrap();
    assert!(Arc::ptr_eq(&registry.list()[0], &a));
    assert!(Arc::ptr_eq(&registry.list()[1], &z));
    assert_eq!(selectors(registry.all_models()), ["a/z", "a/a", "z/m"]);
    assert_eq!(
        registry.resolve_default_canonical().unwrap().selector(),
        "a/a"
    );
    assert!(Arc::ptr_eq(
        &registry.resolve_default_canonical().unwrap().provider,
        &a
    ));
    assert!(registry.models("missing").is_empty());
    assert_eq!(registry.models("orphan")[0].id, "hidden");
    assert!(
        registry
            .all_models()
            .iter()
            .all(|m| m.reasoning().is_none())
    );
    let supplementary = provider("\u{10000}");
    let bmp = provider("\u{e000}");
    let unicode = ProviderRegistry::new(
        vec![bmp, supplementary.clone()],
        BTreeMap::from([(
            "\u{10000}".into(),
            vec![
                Model::new("\u{e000}", "\u{10000}"),
                Model::new("\u{10000}", "\u{10000}"),
            ],
        )]),
    )
    .unwrap();
    assert!(Arc::ptr_eq(&unicode.list()[0], &supplementary));
    assert_eq!(
        unicode.resolve_default_canonical().unwrap().model.id,
        "\u{10000}"
    );
}

#[test]
fn defaults_report_exact_empty_registry_and_first_provider_empty_catalogue_errors() {
    let empty = ProviderRegistry::new(Vec::new(), BTreeMap::new()).unwrap();
    assert_eq!(
        empty.resolve_default_canonical().unwrap_err(),
        Error::Provider("provider: no providers configured".into())
    );
    let missing = ProviderRegistry::new(
        vec![provider("b"), provider("a")],
        BTreeMap::from([("b".into(), vec![Model::new("ready", "b")])]),
    )
    .unwrap();
    assert_eq!(
        missing.resolve_default_canonical().unwrap_err(),
        Error::Provider("provider: \"a\" serves no models".into())
    );
}

#[test]
fn unlisted_variant_fallback_distinguishes_equal_and_conflicting_variant_values() {
    let equal = registry(
        provider("p"),
        vec![
            variant_model("one", "high", "xhigh"),
            variant_model("two", "high", "xhigh"),
        ],
    );
    let selected = equal.resolve_canonical("p/vendor/unlisted/high").unwrap();
    assert_eq!(selected.model.id, "vendor/unlisted");
    assert_eq!(
        selected.variant.unwrap(),
        ModelVariant::new("high", "xhigh")
    );
    let conflicting = registry(
        provider("p"),
        vec![
            variant_model("one", "high", "xhigh"),
            variant_model("two", "high", "high"),
        ],
    );
    let selected = conflicting
        .resolve_canonical("p/vendor/unlisted/high")
        .unwrap();
    assert_eq!(selected.model.id, "vendor/unlisted/high");
    assert!(selected.variant.is_none());
    let unknown = equal
        .resolve_canonical("p/vendor/unlisted/not-known")
        .unwrap();
    assert_eq!(unknown.model.id, "vendor/unlisted/not-known");
    assert!(unknown.variant.is_none());
    let repeated = registry(
        provider("p"),
        vec![
            variant_model("one", "high", "xhigh"),
            variant_model("one", "high", "xhigh"),
        ],
    );
    assert!(matches!(
        repeated.resolve_canonical("p/one/high"),
        Err(Error::Provider(_))
    ));
}

#[tokio::test]
async fn refresh_runs_concurrently_and_joins_other_refreshes_before_propagating_an_error() {
    let barrier = Arc::new(Barrier::new(3));
    let mut first = FakeProvider::new(
        "a",
        true,
        Err(Error::InvalidOperation("not recoverable".into())),
    );
    first.barrier = Some(barrier.clone());
    let release = Arc::new(Notify::new());
    let mut second = FakeProvider::new("b", true, Ok(vec![Model::new("fresh", "b")]));
    second.barrier = Some(barrier.clone());
    second.release = Some(release.clone());
    let first = Arc::new(first);
    let second = Arc::new(second);
    let registry = ProviderRegistry::new(
        vec![first.clone(), second.clone()],
        BTreeMap::from([("b".into(), vec![Model::new("seed", "b")])]),
    )
    .unwrap();
    let mut refresh = registry.available_models(&ACTIVE);
    tokio::select! {
        _=barrier.wait()=>{},
        result=&mut refresh=>panic!("refresh completed before both providers reached barrier: {result:?}"),
        _=tokio::time::sleep(Duration::from_secs(1))=>panic!("refreshes were not concurrent"),
    }
    assert_eq!(first.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.list_calls.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), refresh.as_mut())
            .await
            .is_err()
    );
    release.notify_one();
    assert_eq!(
        refresh.await.unwrap_err(),
        Error::InvalidOperation("not recoverable".into())
    );
    assert_eq!(registry.models("b")[0].id, "seed");
}

#[tokio::test]
async fn refresh_catches_only_source_failure_classes_at_both_provider_boundaries() {
    let swallowed = vec![
        Error::Provider("provider".into()),
        Error::HeaderTimeout("timeout".into()),
        Error::Wire("wire".into()),
        Error::Io("io".into()),
        Error::Http(HttpError::Http("request".into())),
        Error::Http(HttpError::ProviderHttp("provider policy".into())),
        Error::Http(HttpError::Transport("transport".into())),
        Error::Http(HttpError::Provider("provider".into())),
        Error::Http(HttpError::Wire("wire".into())),
        Error::Http(HttpError::HeaderTimeout(Duration::from_secs(1))),
        Error::Http(HttpError::RequestTooLarge(10)),
        Error::Http(HttpError::Response(HttpFailure {
            status: 503,
            error_type: String::new(),
            error_code: String::new(),
            detail: String::new(),
            response_body: String::new(),
        })),
        Error::Response(ResponsesError::WireProtocol("wire".into())),
        Error::Response(ResponsesError::ProviderResponse {
            error_type: String::new(),
            error_code: String::new(),
            detail: String::new(),
            response_body: String::new(),
        }),
    ];
    let propagated = vec![
        Error::Cancelled,
        Error::Cancellation("custom cancellation".into()),
        Error::InvalidOperation("bug".into()),
        Error::Disposed,
        Error::Encode("json".into()),
        Error::Http(HttpError::Cancelled),
        Error::Http(HttpError::RequestCancelled(
            "The operation was canceled.".into(),
        )),
        Error::Http(HttpError::Disposed),
        Error::Http(HttpError::InvalidJsonString("json".into())),
        Error::Response(ResponsesError::Json("json".into())),
        Error::Response(ResponsesError::InvalidInteger {
            field: "n".into(),
            value: "bad".into(),
        }),
        Error::Socket(WebSocketError::Transport("socket".into())),
    ];
    for error in swallowed {
        for credentials in [true, false] {
            let fake = Arc::new(FakeProvider::new("p", true, Err(error.clone())));
            if credentials {
                *fake.credentials.lock().unwrap() = Err(error.clone());
            }
            let registry = registry(fake.clone(), vec![Model::new("seed", "p")]);
            let actual = registry.available_models(&ACTIVE).await.unwrap();
            assert_eq!(actual.len(), usize::from(!credentials), "{error:?}");
            assert_eq!(
                fake.list_calls.load(Ordering::SeqCst),
                usize::from(!credentials)
            );
            assert_eq!(registry.models("p")[0].id, "seed");
        }
    }
    for error in propagated {
        for credentials in [true, false] {
            let fake = Arc::new(FakeProvider::new("p", true, Err(error.clone())));
            if credentials {
                *fake.credentials.lock().unwrap() = Err(error.clone());
            }
            let registry = registry(fake, vec![Model::new("seed", "p")]);
            assert_eq!(registry.available_models(&ACTIVE).await.unwrap_err(), error);
            assert_eq!(registry.models("p")[0].id, "seed");
        }
    }
}

#[tokio::test]
async fn authentication_refresh_uses_actual_enum_categories_and_preserves_native_failures() {
    use parrot_foundation::{credentials::CredentialError, oauth::AuthError};

    let caught = [
        AuthError::Message("auth failure"),
        AuthError::Http {
            operation: "refresh",
            status: 401,
        },
        AuthError::Credential(CredentialError::Malformed),
        AuthError::BrowserLaunch(None),
        AuthError::Transport,
        AuthError::Io,
    ];
    let native = [
        AuthError::InvalidOperation("native invalid operation"),
        AuthError::Format,
        AuthError::ArgumentOutOfRange("native range"),
        AuthError::NotSupported("native unsupported"),
    ];
    for (errors, caught) in [(caught.as_slice(), true), (native.as_slice(), false)] {
        for auth_error in errors {
            let error = Error::Authentication(auth_error.clone());
            for credential_failure in [true, false] {
                let fake = Arc::new(FakeProvider::new("p", true, Err(error.clone())));
                if credential_failure {
                    *fake.credentials.lock().unwrap() = Err(error.clone());
                }
                let registry = registry(fake.clone(), vec![Model::new("seed", "p")]);
                let actual = registry.available_models(&ACTIVE).await;
                if caught {
                    assert_eq!(actual.unwrap().len(), usize::from(!credential_failure));
                } else {
                    assert_eq!(actual.unwrap_err(), error);
                }
                assert_eq!(
                    fake.list_calls.load(Ordering::SeqCst),
                    usize::from(!credential_failure)
                );
                assert_eq!(registry.models("p")[0].id, "seed");
            }
        }
    }
    // The outer source LLMProviderException is caught regardless of its retained cause.
    let wrapped = Error::ProviderWithCause {
        message: "provider: cannot obtain authentication".into(),
        cause: Box::new(Error::Authentication(AuthError::Format)),
    };
    for credential_failure in [true, false] {
        let fake = Arc::new(FakeProvider::new("p", true, Err(wrapped.clone())));
        if credential_failure {
            *fake.credentials.lock().unwrap() = Err(wrapped.clone());
        }
        let registry = registry(fake, vec![Model::new("seed", "p")]);
        assert_eq!(
            registry.available_models(&ACTIVE).await.unwrap().len(),
            usize::from(!credential_failure)
        );
        assert_eq!(registry.models("p")[0].id, "seed");
    }
}

#[tokio::test]
async fn caller_cancellation_is_forwarded_without_an_extra_registry_cancellation_policy() {
    let mut aware = FakeProvider::new("p", true, Ok(vec![Model::new("fresh", "p")]));
    aware.observe_cancellation = true;
    let aware = Arc::new(aware);
    let registry = registry(aware.clone(), vec![Model::new("seed", "p")]);
    assert_eq!(
        registry.available_models(&Token(true)).await.unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(aware.list_calls.load(Ordering::SeqCst), 0);
    // A ready source collaborator may ignore its cancellation token, as in .NET.
    let ignoring = registry_for_ignoring_provider();
    assert_eq!(
        selectors(ignoring.available_models(&Token(true)).await.unwrap()),
        ["p/fresh"]
    );
}
fn registry_for_ignoring_provider() -> ProviderRegistry {
    registry(
        Arc::new(FakeProvider::new(
            "p",
            true,
            Ok(vec![Model::new("fresh", "p")]),
        )),
        vec![Model::new("seed", "p")],
    )
}

#[tokio::test]
async fn successful_empty_refresh_replaces_seed_and_returned_catalogue_copies_are_independent() {
    let fake = Arc::new(FakeProvider::new("p", true, Ok(Vec::new())));
    let registry = registry(fake, vec![Model::new("seed", "p")]);
    let mut copy = registry.models("p");
    copy[0].id = "changed".into();
    assert_eq!(registry.models("p")[0].id, "seed");
    assert!(registry.available_models(&ACTIVE).await.unwrap().is_empty());
    assert!(registry.models("p").is_empty());
}
