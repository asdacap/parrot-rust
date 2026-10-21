use crate::support::Context;
#[test]
fn source_assertions() {
    let yaml = "providers:\n  openrouter:\n    provider_preferences:\n      allow_fallbacks: false";
    let context = Context::new();
    let configuration = context.configured(yaml).unwrap();
    let preferences: serde_json::Value =
        serde_json::from_str(&configuration.models.providers["openrouter"].provider_preferences)
            .unwrap();
    assert_eq!(preferences.as_object().unwrap().len(), 4);
    assert_eq!(preferences["allow_fallbacks"], false);
    assert_eq!(preferences["require_parameters"], true);
    assert_eq!(preferences["data_collection"], "deny");
    assert_eq!(preferences["zdr"], true);
    assert_eq!(context.saved(), yaml);
}
