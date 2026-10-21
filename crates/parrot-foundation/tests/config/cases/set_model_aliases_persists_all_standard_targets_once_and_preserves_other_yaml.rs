use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    let mut configuration = context.configured("theme: dark\nmodel_aliases:\n  low_llm:\n    usage: Customized low usage\n    augment_system_prompt: Keep this metadata.\n  custom:\n    model_string: local/custom\n    usage: Custom work\n").unwrap();
    let defaults = configuration.models.provider_model_alias_defaults["chatgpt"].clone();
    configuration.set_model_aliases(&defaults).unwrap();
    let reloaded = context.load().unwrap();
    let rewritten = context.saved();
    for (id, target) in defaults.targets() {
        assert_eq!(configuration.models.model_aliases[&id].model_string, target);
        assert_eq!(reloaded.models.model_aliases[&id].model_string, target);
    }
    assert_eq!(
        reloaded.models.model_aliases["low_llm"].usage,
        "Customized low usage"
    );
    assert_eq!(
        reloaded.models.model_aliases["custom"].model_string,
        "local/custom"
    );
    assert!(rewritten.contains("theme: dark"));
    assert!(rewritten.contains("augment_system_prompt: Keep this metadata."));
    assert_eq!(rewritten.split("model_string: chatgpt/").count(), 5);
}
