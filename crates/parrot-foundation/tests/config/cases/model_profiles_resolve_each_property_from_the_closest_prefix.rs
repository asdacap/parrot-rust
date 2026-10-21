use crate::support::Context;
use parrot_domain::values::config::ModelProfileConfig;
#[test]
fn source_assertions() {
    let context = Context::new();
    let profiles = context
        .configured(
            r#"model_profiles:
  provider:
    usage: Provider usage
    system_prompt: Provider prompt
  provider/vendor/model:
    system_prompt: Model prompt
  provider/vendor/model/high:
    usage: Variant usage
    system_prompt: null
  provider/vendor/model/low:
    usage: ''
    system_prompt: ''
"#,
        )
        .unwrap()
        .models
        .model_profiles;
    assert_eq!(
        profiles.resolve("provider/vendor/model/high"),
        ModelProfileConfig::new(
            Some("Variant usage".into()),
            Some("Model prompt".into()),
            None,
            None
        )
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model"),
        ModelProfileConfig::new(
            Some("Provider usage".into()),
            Some("Model prompt".into()),
            None,
            None
        )
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model/low"),
        ModelProfileConfig::new(Some("".into()), Some("".into()), None, None)
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model60"),
        ModelProfileConfig::new(
            Some("Provider usage".into()),
            Some("Provider prompt".into()),
            None,
            None
        )
    );
    assert_eq!(
        profiles.resolve("Provider/vendor/model"),
        ModelProfileConfig::new(None, None, None, None)
    );
    assert_eq!(
        profiles.resolve("unknown/model"),
        ModelProfileConfig::new(None, None, None, None)
    );
}
