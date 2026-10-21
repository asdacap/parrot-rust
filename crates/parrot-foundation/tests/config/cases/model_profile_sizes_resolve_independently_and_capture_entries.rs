use crate::support::Context;
use parrot_domain::values::config::{ModelProfileConfig, ModelProfiles};
use std::collections::BTreeMap;

#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("model_profiles:\n  provider:\n    maximum_input_tokens: 80000\n  provider/model:\n    summary_output_tokens: 10000\n").unwrap();
    assert_eq!(
        configuration
            .models
            .model_profiles
            .resolve("provider/model"),
        ModelProfileConfig::new(None, None, Some(80000), Some(10000))
    );
    let original = ModelProfileConfig::new(None, None, Some(80000), Some(14000));
    let mut entries = BTreeMap::from([("provider".into(), original.clone())]);
    let profiles = ModelProfiles::new(&entries);
    entries.insert(
        "provider".into(),
        ModelProfileConfig::new(None, None, Some(40000), Some(7000)),
    );
    assert_eq!(profiles.resolve("provider/model"), original);
}
