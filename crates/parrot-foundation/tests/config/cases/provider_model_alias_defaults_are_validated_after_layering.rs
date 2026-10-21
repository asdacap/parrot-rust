use crate::support::Context;
use parrot_foundation::config::models::ProviderModelAliasDefaults;

#[test]
fn source_assertions() {
    let context = Context::new();
    let baseline = context
        .configured("")
        .unwrap()
        .models
        .provider_model_alias_defaults;
    let defaults = context.configured("provider_model_alias_defaults:\n  chatgpt:\n    low_llm: chatgpt/custom-low\n  local:\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: local/xhigh\n").unwrap().models.provider_model_alias_defaults;
    assert_eq!(defaults["chatgpt"].low_model_string, "chatgpt/custom-low");
    assert_eq!(
        defaults["chatgpt"].xhigh_model_string,
        baseline["chatgpt"].xhigh_model_string
    );
    assert_eq!(
        defaults["local"],
        ProviderModelAliasDefaults {
            provider_id: "local".into(),
            low_model_string: "local/low".into(),
            medium_model_string: "local/medium".into(),
            high_model_string: "local/high".into(),
            xhigh_model_string: "local/xhigh".into()
        }
    );
}
