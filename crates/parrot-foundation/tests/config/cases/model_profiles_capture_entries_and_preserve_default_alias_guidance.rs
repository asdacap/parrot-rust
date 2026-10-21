use crate::support::Context;
use parrot_domain::values::config::{ModelProfileConfig, ModelProfiles};
use std::collections::BTreeMap;
#[test]
fn source_assertions() {
    let original = ModelProfileConfig::new(
        Some("Provider usage".into()),
        Some("Provider prompt".into()),
        None,
        None,
    );
    let mut entries = BTreeMap::from([("provider".into(), original.clone())]);
    let profiles = ModelProfiles::new(&entries);
    entries.insert(
        "provider".into(),
        ModelProfileConfig::new(
            Some("Changed usage".into()),
            Some("Changed prompt".into()),
            None,
            None,
        ),
    );
    assert_eq!(profiles.resolve("provider/model"), original);
    let context = Context::new();
    let defaults = context.configured("").unwrap().models.model_aliases;
    let configuration = context.configured("model_aliases:\n  low_llm:\n    model_string: provider/model\nmodel_profiles:\n  provider/model:\n    usage: Model usage\n").unwrap();
    assert_eq!(
        configuration.models.model_aliases["low_llm"].usage,
        defaults["low_llm"].usage
    );
    assert_eq!(
        configuration.models.model_aliases["low_llm"].model_string,
        "provider/model"
    );
    assert_eq!(
        configuration
            .models
            .model_profiles
            .resolve("provider/model"),
        ModelProfileConfig::new(Some("Model usage".into()), None, None, None)
    );
}
