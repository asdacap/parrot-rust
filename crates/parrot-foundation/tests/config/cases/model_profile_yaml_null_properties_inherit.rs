use crate::support::ConfigFixture;
use parrot_domain::values::config::ModelProfileConfig;
#[test]
fn source_assertions() {
    for value in ["null", "Null", "NULL", "~", ""] {
        let fixture = ConfigFixture::new();
        let configuration=fixture.configured(&format!("model_profiles:\n  provider:\n    usage: Parent usage\n    system_prompt: Parent prompt\n  provider/model:\n    usage: {value}\n    system_prompt: {value}\n")).unwrap();
        assert_eq!(
            configuration
                .models
                .model_profiles
                .resolve("provider/model"),
            ModelProfileConfig::new(
                Some("Parent usage".into()),
                Some("Parent prompt".into()),
                None,
                None
            )
        );
    }
}
