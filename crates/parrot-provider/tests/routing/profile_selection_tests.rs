//! T32 composition proof for ModelPrompt.cs:26,40; profile resolution stays T09-owned.
use super::alias_tests::{registry, routing};
use parrot_domain::values::config::{ModelProfileConfig, ModelProfiles};
use parrot_provider::{
    catalogs::{Model, ModelVariant},
    routing::{aliases::ModelAliasDefinition, registry::ProviderRegistry},
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn alias_default_and_slash_effort_selection_feed_canonical_selector_to_domain_profiles() {
    let mut model = Model::new("vendor/model", "p");
    model.capabilities.variants = vec![ModelVariant::new("high", "xhigh")];
    let registry = Arc::new(
        ProviderRegistry::new(
            registry(&[]).list(),
            BTreeMap::from([("p".into(), vec![model])]),
        )
        .unwrap(),
    );
    let (_, router) = routing(
        registry,
        "preferred",
        vec![ModelAliasDefinition::new(
            "preferred",
            "p/vendor/model/high",
            "alias usage",
        )],
    );
    let profiles = ModelProfiles::new(&BTreeMap::from([
        (
            "p".into(),
            ModelProfileConfig::new(
                Some("provider usage".into()),
                Some("provider prompt".into()),
                Some(4096),
                Some(64),
            ),
        ),
        (
            "p/vendor".into(),
            ModelProfileConfig::new(Some("vendor usage".into()), None, None, Some(128)),
        ),
        (
            "p/vendor/model".into(),
            ModelProfileConfig::new(
                Some("model usage".into()),
                Some("model prompt".into()),
                None,
                None,
            ),
        ),
        (
            "p/vendor/model/high".into(),
            ModelProfileConfig::new(Some(String::new()), None, None, Some(256)),
        ),
        (
            "p/vendor/model/xhigh".into(),
            ModelProfileConfig::new(
                Some("effort spelling must not select this".into()),
                None,
                None,
                None,
            ),
        ),
        (
            "preferred".into(),
            ModelProfileConfig::new(
                Some("alias spelling must not select this".into()),
                Some("alias prompt must not select this".into()),
                Some(9999),
                Some(9999),
            ),
        ),
    ]));
    let expected_variant = ModelProfileConfig::new(
        Some(String::new()),
        Some("model prompt".into()),
        Some(4096),
        Some(256),
    );
    for requested in ["", "preferred", "p/vendor/model/high"] {
        let selection = router.resolve(requested).unwrap();
        assert_eq!(selection.canonical_model.selector(), "p/vendor/model/high");
        assert_eq!(
            selection.canonical_model.reasoning().unwrap().effort,
            "xhigh"
        );
        assert_eq!(selection.canonical_base(), "p/vendor/model");
        assert_eq!(
            profiles.resolve(&selection.canonical_model.selector()),
            expected_variant,
        );
        if requested != "p/vendor/model/high" {
            assert_eq!(selection.requested_selector.value, "preferred");
            assert_eq!(selection.alias.unwrap().name, "preferred");
        }
    }
    let base = router.resolve("p/vendor/model").unwrap();
    assert!(base.canonical_model.variant.is_none());
    assert_eq!(
        profiles.resolve(&base.canonical_model.selector()),
        ModelProfileConfig::new(
            Some("model usage".into()),
            Some("model prompt".into()),
            Some(4096),
            Some(128),
        ),
    );
    assert_ne!(profiles.resolve("preferred"), expected_variant);
}

#[test]
fn alias_retarget_changes_future_profile_selection_but_captured_snapshot_keeps_old_profile() {
    let mut old = Model::new("vendor/old", "p");
    old.capabilities.variants = vec![ModelVariant::new("high", "xhigh")];
    let mut new = Model::new("vendor/new", "p");
    new.capabilities.variants = vec![ModelVariant::new("low", "minimal")];
    let registry = Arc::new(
        ProviderRegistry::new(
            registry(&[]).list(),
            BTreeMap::from([("p".into(), vec![old, new])]),
        )
        .unwrap(),
    );
    let (routing, router) = routing(
        registry,
        "preferred",
        vec![ModelAliasDefinition::new(
            "preferred",
            "p/vendor/old/high",
            "primary",
        )],
    );
    let profiles = ModelProfiles::new(&BTreeMap::from([
        (
            "p".into(),
            ModelProfileConfig::new(
                Some("inherited provider usage".into()),
                None,
                Some(8192),
                None,
            ),
        ),
        (
            "p/vendor/old/high".into(),
            ModelProfileConfig::new(None, Some("old profile".into()), None, Some(512)),
        ),
        (
            "p/vendor/new/low".into(),
            ModelProfileConfig::new(None, Some("new profile".into()), None, Some(128)),
        ),
        (
            "preferred".into(),
            ModelProfileConfig::new(None, Some("alias must not select this".into()), None, None),
        ),
    ]));
    let captured = router.resolve("").unwrap();
    let old_profile = profiles.resolve(&captured.canonical_model.selector());
    routing.publish(
        routing
            .prepare(
                "preferred",
                vec![ModelAliasDefinition::new(
                    "preferred",
                    "p/vendor/new/low",
                    "primary",
                )],
            )
            .unwrap(),
    );
    let current = router.resolve("preferred").unwrap();
    let current_profile = profiles.resolve(&current.canonical_model.selector());
    assert_eq!(captured.canonical_model.selector(), "p/vendor/old/high");
    assert_eq!(captured.canonical_base(), "p/vendor/old");
    assert_eq!(captured.routing_snapshot.revision, 0);
    assert_eq!(
        captured
            .alias_snapshot()
            .find("preferred")
            .unwrap()
            .model_string,
        "p/vendor/old/high"
    );
    assert_eq!(current.canonical_model.selector(), "p/vendor/new/low");
    assert_eq!(current.canonical_base(), "p/vendor/new");
    assert_eq!(current.routing_snapshot.revision, 1);
    assert_eq!(
        old_profile,
        ModelProfileConfig::new(
            Some("inherited provider usage".into()),
            Some("old profile".into()),
            Some(8192),
            Some(512)
        )
    );
    assert_eq!(
        current_profile,
        ModelProfileConfig::new(
            Some("inherited provider usage".into()),
            Some("new profile".into()),
            Some(8192),
            Some(128)
        )
    );
    assert_eq!(
        profiles.resolve(&captured.canonical_model.selector()),
        old_profile
    );
    let from_old_snapshot = router
        .resolve_from(captured.routing_snapshot.clone(), "preferred")
        .unwrap();
    assert_eq!(
        profiles.resolve(&from_old_snapshot.canonical_model.selector()),
        old_profile
    );
}
