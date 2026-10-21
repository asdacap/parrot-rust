use super::*;

struct AmbiguousAfterRefresh;
impl Provider for AmbiguousAfterRefresh {
    fn id(&self) -> &str {
        "unused"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        UnusedProvider.call(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async {
            let mut first = Model::new("vendor/model", "unused");
            first
                .capabilities
                .variants
                .push(ModelVariant::new("high", "high"));
            Ok(vec![first.clone(), first])
        })
    }
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        UnusedProvider.calculate_image_tokens(model, content)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        UnusedProvider.open_session()
    }
}
struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn alias_usage_survives_a_real_route_failure_and_cache_uses_snapshot_identity() {
    let mut original_model = Model::new("vendor/model", "unused");
    original_model
        .capabilities
        .variants
        .push(ModelVariant::new("high", "high"));
    let registry = Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(AmbiguousAfterRefresh)],
            BTreeMap::from([("unused".into(), vec![original_model])]),
        )
        .unwrap(),
    );
    let catalog = Arc::new(
        ModelAliasCatalog::new(
            registry.clone(),
            vec![ModelAliasDefinition::new(
                "alpha",
                "unused/vendor/model/high",
                "Alias usage",
            )],
        )
        .unwrap(),
    );
    let routing = Arc::new(ModelRouting::new(catalog, "unused/vendor/model"));
    let router = Arc::new(ModelRouter::new(registry.clone(), routing));
    let selection = router.resolve("unused/vendor/model").unwrap();
    let provider = ModelPromptProvider::new(
        profiles(&[("unused/vendor/model/high", Some("Variant usage"), None)]),
        router.clone(),
        templates(""),
    );
    let mut prompt = provider.materialize(&identity()).unwrap();
    let before = prompt.build(&selection).unwrap();
    assert!(before.contains("Alias usage\nVariant usage"));
    // The refresh is a real registry operation over an explicitly local provider;
    // no network, catalog text or ModelPrompt output is substituted.
    registry.available_models(&NeverCancelled).await.unwrap();
    assert!(
        router
            .resolve_from(selection.routing_snapshot.clone(), "alpha")
            .is_err()
    );
    prompt.renew_epoch();
    assert_eq!(prompt.build(&selection).unwrap(), before);
    let mut next = selection.clone();
    next.routing_snapshot = Arc::new((*selection.routing_snapshot).clone());
    assert_eq!(
        next.routing_snapshot.revision,
        selection.routing_snapshot.revision
    );
    let refreshed = prompt.build(&next).unwrap();
    assert!(refreshed.contains("Alias usage"));
    assert!(!refreshed.contains("Variant usage"));
    // Per-agent products do not inherit another agent's cached alias text.
    assert_eq!(
        provider
            .materialize(&identity())
            .unwrap()
            .build(&selection)
            .unwrap(),
        refreshed
    );
}
