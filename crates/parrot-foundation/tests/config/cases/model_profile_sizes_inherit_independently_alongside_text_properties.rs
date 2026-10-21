use crate::support::Context;
use parrot_domain::values::config::ModelProfileConfig;
#[test]
fn source_assertions() {
    let context = Context::new();
    let profiles = context
        .configured(
            r#"model_profiles:
  provider:
    maximum_input_tokens: 90000
    summary_output_tokens: 15000
  provider/vendor/model:
    maximum_input_tokens: 70000
  provider/vendor/model/high:
    usage: Variant usage
    system_prompt: Variant prompt
    summary_output_tokens: 10000
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
            Some("Variant prompt".into()),
            Some(70000),
            Some(10000)
        )
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model/low"),
        ModelProfileConfig::new(Some("".into()), Some("".into()), Some(70000), Some(15000))
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model"),
        ModelProfileConfig::new(None, None, Some(70000), Some(15000))
    );
    assert_eq!(
        profiles.resolve("provider/vendor/model60"),
        ModelProfileConfig::new(None, None, Some(90000), Some(15000))
    );
    assert_eq!(
        profiles.resolve("Provider/vendor/model/high"),
        ModelProfileConfig::new(None, None, None, None)
    );
    assert_eq!(
        profiles.resolve("unknown/model"),
        ModelProfileConfig::new(None, None, None, None)
    );
}
