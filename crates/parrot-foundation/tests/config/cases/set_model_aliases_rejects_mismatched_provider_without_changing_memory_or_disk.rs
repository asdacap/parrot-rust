use crate::support::Context;
use parrot_foundation::config::models::ProviderModelAliasDefaults;

#[test]
fn source_assertions() {
    let context = Context::new();
    let mut configuration = context.configured("theme: dark\n").unwrap();
    let before = context.saved();
    let defaults = ProviderModelAliasDefaults {
        provider_id: "chatgpt".into(),
        low_model_string: "chatgpt/low".into(),
        medium_model_string: "chatgpt/medium".into(),
        high_model_string: "other/high".into(),
        xhigh_model_string: "chatgpt/xhigh".into(),
    };
    assert!(configuration.set_model_aliases(&defaults).is_err());
    assert!(
        configuration.models.model_aliases["low_llm"]
            .model_string
            .is_empty()
    );
    assert_eq!(context.saved(), before);
}
