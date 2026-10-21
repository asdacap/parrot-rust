use crate::support::Context;
#[test]
fn source_assertions() {
    for (content, field) in [
        ("model_profiles: []", "model_profiles"),
        ("model_profiles: null", "model_profiles"),
        ("model_profiles:\n  '': {}", "model_profiles"),
        ("model_profiles:\n  ' provider': {}", "model_profiles"),
        ("model_profiles:\n  'provider ': {}", "model_profiles"),
        ("model_profiles:\n  provider//model: {}", "model_profiles"),
        ("model_profiles:\n  /provider: {}", "model_profiles"),
        ("model_profiles:\n  provider/: {}", "model_profiles"),
        (
            "model_profiles:\n  \"provider\\tmodel\": {}",
            "model_profiles",
        ),
        (
            "model_profiles:\n  provider: scalar",
            "model_profiles.provider",
        ),
        (
            "model_profiles:\n  provider:\n    unknown: value",
            "model_profiles.provider",
        ),
        (
            "model_profiles:\n  provider:\n    usage: []",
            "model_profiles.provider.usage",
        ),
        (
            "model_profiles:\n  provider:\n    system_prompt: {}",
            "model_profiles.provider.system_prompt",
        ),
    ] {
        let context = Context::new();
        assert!(
            context
                .configured(content)
                .unwrap_err()
                .to_string()
                .contains(field)
        );
    }
}
