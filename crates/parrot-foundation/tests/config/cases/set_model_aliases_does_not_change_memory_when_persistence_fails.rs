use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config = context.load().unwrap();
    let defaults = config.models.provider_model_alias_defaults["chatgpt"].clone();
    std::fs::create_dir(&context.path).unwrap();
    assert!(config.set_model_aliases(&defaults).is_err());
    for alias in ["low_llm", "medium_llm", "high_llm", "xhigh_llm"] {
        assert!(config.models.model_aliases[alias].model_string.is_empty());
    }
}
