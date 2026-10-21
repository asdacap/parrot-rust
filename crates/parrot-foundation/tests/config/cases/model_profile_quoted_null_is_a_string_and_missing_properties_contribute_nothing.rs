use crate::support::Context;
use parrot_domain::values::config::ModelProfileConfig;

#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("model_profiles:\n  provider/model:\n    usage: 'null'\n    system_prompt: '~'\n  provider/empty: {}\n").unwrap();
    assert_eq!(
        configuration
            .models
            .model_profiles
            .resolve("provider/model"),
        ModelProfileConfig::new(Some("null".into()), Some("~".into()), None, None)
    );
    assert_eq!(
        configuration
            .models
            .model_profiles
            .resolve("provider/empty"),
        ModelProfileConfig::default()
    );
    assert_eq!(
        context
            .configured("")
            .unwrap()
            .models
            .model_profiles
            .resolve("provider/model"),
        ModelProfileConfig::default()
    );
}
