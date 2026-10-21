use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    routing::{aliases::*, registry::ProviderRegistry},
    sessions::{Content, Error, EventStream, Provider, Request, Session},
};
use std::{collections::BTreeMap, sync::Arc};

struct FakeProvider;
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "p"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Err(Error::InvalidOperation("not called".into()))
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
        Box::pin(async { Ok(Vec::new()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Err(Error::InvalidOperation("not called".into()))
    }
}
pub(super) fn registry(models: &[&str]) -> Arc<ProviderRegistry> {
    Arc::new(
        ProviderRegistry::new(
            vec![Arc::new(FakeProvider)],
            BTreeMap::from([(
                "p".into(),
                models.iter().map(|id| Model::new(*id, "p")).collect(),
            )]),
        )
        .unwrap(),
    )
}
pub(super) fn routing(
    registry: Arc<ProviderRegistry>,
    default: &str,
    aliases: Vec<ModelAliasDefinition>,
) -> (Arc<ModelRouting>, Arc<ModelRouter>) {
    let catalog = Arc::new(ModelAliasCatalog::new(registry.clone(), aliases).unwrap());
    let routing = Arc::new(ModelRouting::new(catalog, default));
    let router = Arc::new(ModelRouter::new(registry, routing.clone()));
    (routing, router)
}

#[test]
fn resolve_preserves_requested_alias_identity_and_canonical_slash_model_route() {
    let registry = registry(&["vendor/model"]);
    let mut alias = ModelAliasDefinition::new("preferred", "p/vendor/model", "primary");
    alias.augment_system_prompt = Some("system prompt".into());
    let (_, router) = routing(registry, "", vec![alias]);
    let selection = router.resolve("preferred").unwrap();
    assert_eq!(selection.requested_selector.value, "preferred");
    assert_eq!(selection.alias.as_ref().unwrap().name, "preferred");
    assert_eq!(selection.canonical_model.selector(), "p/vendor/model");
    assert_eq!(selection.canonical_base(), "p/vendor/model");
}
#[test]
fn resolve_uses_default_alias_and_accepts_unlisted_alias_target() {
    let (_, router) = routing(
        registry(&["listed"]),
        "default",
        vec![
            ModelAliasDefinition::new("default", "p/not-listed", "primary"),
            ModelAliasDefinition::new("unlisted", "p/also-not-listed", "secondary"),
        ],
    );
    let default = router.resolve("").unwrap();
    assert_eq!(default.alias.unwrap().name, "default");
    assert_eq!(default.canonical_model.selector(), "p/not-listed");
    assert_eq!(
        router
            .resolve("unlisted")
            .unwrap()
            .canonical_model
            .selector(),
        "p/also-not-listed"
    );
}
#[test]
fn resolve_rejects_disabled_alias() {
    let (_, router) = routing(
        registry(&["listed"]),
        "",
        vec![ModelAliasDefinition::new("disabled", "", "primary")],
    );
    assert_eq!(
        router.resolve("disabled").err().unwrap(),
        Error::Provider("model alias: alias \"disabled\" is not configured".into())
    );
}
#[test]
fn replace_retargets_future_resolutions_while_captured_snapshot_keeps_old_alias() {
    let (routing, router) = routing(
        registry(&["old", "new"]),
        "",
        vec![ModelAliasDefinition::new("preferred", "p/old", "primary")],
    );
    let before = router.resolve("preferred").unwrap();
    routing.publish(
        routing
            .prepare(
                "",
                vec![ModelAliasDefinition::new("preferred", "p/new", "primary")],
            )
            .unwrap(),
    );
    let after = router.resolve("preferred").unwrap();
    assert_eq!(before.canonical_model.selector(), "p/old");
    assert_eq!(
        before
            .alias_snapshot()
            .find("preferred")
            .unwrap()
            .model_string,
        "p/old"
    );
    assert_eq!(after.canonical_model.selector(), "p/new");
    assert_eq!(
        after
            .alias_snapshot()
            .find("preferred")
            .unwrap()
            .model_string,
        "p/new"
    );
    assert_eq!(before.routing_snapshot.revision, 0);
    assert_eq!(after.routing_snapshot.revision, 1);
}
#[test]
fn replacement_rejections_are_atomic_and_disallow_self_or_chained_aliases() {
    let registry = registry(&["old"]);
    let catalog = Arc::new(
        ModelAliasCatalog::new(
            registry.clone(),
            vec![ModelAliasDefinition::new("preferred", "p/old", "primary")],
        )
        .unwrap(),
    );
    assert!(matches!(
        ModelAliasCatalog::new(
            registry.clone(),
            vec![ModelAliasDefinition::new("self", "self", "primary")]
        ),
        Err(Error::Provider(_))
    ));
    assert!(matches!(
        ModelAliasCatalog::new(
            registry,
            vec![
                ModelAliasDefinition::new("first", "second", "primary"),
                ModelAliasDefinition::new("second", "p/old", "primary")
            ]
        ),
        Err(Error::Provider(_))
    ));
    let routing = ModelRouting::new(catalog, "");
    let old = routing.capture();
    assert!(matches!(
        routing.prepare(
            "",
            vec![ModelAliasDefinition::new(
                "preferred",
                "preferred",
                "primary"
            )]
        ),
        Err(Error::Provider(_))
    ));
    assert!(Arc::ptr_eq(&old, &routing.capture()));
    assert_eq!(
        routing
            .capture()
            .aliases
            .find("preferred")
            .unwrap()
            .model_string,
        "p/old"
    );
}
#[test]
fn alias_validation_keeps_exact_source_order_and_messages() {
    let registry = registry(&["old"]);
    let cases = [
        (
            vec![ModelAliasDefinition::new("", "p/old", "primary")],
            "model alias: alias name is required",
        ),
        (
            vec![ModelAliasDefinition::new(" a", "p/old", "primary")],
            "model alias: alias name \" a\" has surrounding whitespace",
        ),
        (
            vec![ModelAliasDefinition::new("a/b", "p/old", "primary")],
            "model alias: alias name \"a/b\" contains '/'",
        ),
        (
            vec![ModelAliasDefinition::new("a", "p/old", "")],
            "model alias: usage is required for \"a\"",
        ),
        (
            vec![
                ModelAliasDefinition::new("a", "p/old", "primary"),
                ModelAliasDefinition::new("a", "p/new", ""),
            ],
            "model alias: duplicate alias \"a\"",
        ),
        (
            vec![ModelAliasDefinition::new("a", "q/old", "primary")],
            "model alias: invalid target for \"a\": provider: unknown provider \"q\"",
        ),
    ];
    for (aliases, message) in cases {
        assert_eq!(
            ModelAliasCatalog::new(registry.clone(), aliases)
                .err()
                .unwrap(),
            Error::Provider(message.into())
        );
    }
}
#[test]
fn empty_request_uses_canonical_default_and_explicit_request_overrides_configured_alias() {
    let (_, router) = routing(registry(&["z", "a"]), "", vec![]);
    let result = router.resolve("").unwrap();
    assert_eq!(result.requested_selector.value, "p/a");
    assert!(result.alias.is_none());
    let (_, router) = routing(
        registry(&["listed"]),
        "disabled",
        vec![ModelAliasDefinition::new("disabled", "", "primary")],
    );
    assert_eq!(
        router
            .resolve("p/explicit")
            .unwrap()
            .canonical_model
            .selector(),
        "p/explicit"
    );
}
#[test]
fn alias_snapshot_uses_source_utf16_ordinal_order() {
    let catalog = ModelAliasCatalog::new(
        registry(&["old"]),
        vec![
            ModelAliasDefinition::new("\u{e000}", "p/old", "private use"),
            ModelAliasDefinition::new("😀", "p/old", "astral"),
            ModelAliasDefinition::new("a", "p/old", "ascii"),
        ],
    )
    .unwrap();
    assert_eq!(
        catalog
            .initial_snapshot()
            .definitions()
            .iter()
            .map(|alias| alias.name.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "😀", "\u{e000}"]
    );
}

#[test]
fn snapshots_preserve_metadata_context_precedence_and_explicit_revision() {
    let registry = registry(&["old"]);
    let mut alias = ModelAliasDefinition::new("preferred", "p/old", "primary");
    alias.context_limit = Some(parrot_domain::values::context::ContextSize::parse("20%").unwrap());
    alias.icon = Some(ModelAliasIcon::parse("★", "cyan").unwrap());
    alias.augment_system_prompt = Some("prompt".into());
    let (routing, router) = routing(registry, "preferred", vec![alias.clone()]);
    let mut prepared = (*routing
        .prepare_with_revision("preferred", vec![alias], 41)
        .unwrap())
    .clone();
    prepared.context_limit =
        Some(parrot_domain::values::context::ContextSize::parse("100k").unwrap());
    routing.publish(Arc::new(prepared));
    let selected = router.resolve("").unwrap();
    assert_eq!(router.routing_revision(), 41);
    assert_eq!(selected.context_limit().unwrap().to_string(), "20%");
    assert_eq!(
        selected.alias.unwrap().icon.unwrap().color,
        ModelAliasIconColor::Cyan
    );
    assert!(ModelSelector::new("").is_err());
    assert_eq!(ModelSelector::new("a").unwrap().to_string(), "a");
    assert_eq!(
        ModelAliasIcon::parse("★", "Cyan").unwrap_err().to_string(),
        "unsupported model alias icon color Cyan"
    );
}
