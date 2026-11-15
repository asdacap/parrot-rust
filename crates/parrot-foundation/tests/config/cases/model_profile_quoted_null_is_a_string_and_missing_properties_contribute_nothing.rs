use crate::support::ConfigFixture;
use parrot_domain::values::config::ModelProfileConfig;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture.configured("model_profiles:\n  provider/model:\n    usage: 'null'\n    system_prompt: '~'\n  provider/empty: {}\n").unwrap();
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
        fixture
            .configured("")
            .unwrap()
            .models
            .model_profiles
            .resolve("provider/model"),
        ModelProfileConfig::default()
    );
}
