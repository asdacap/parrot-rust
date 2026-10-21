use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "provider_model_alias_defaults: []\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: other/xhigh\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: local/xhigh\n    extra: local/extra\n",
        "provider_model_alias_defaults:\n  ' local ':\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: local/xhigh\n",
    ] {
        let context = Context::new();
        let _: parrot_foundation::config::ConfigError = context.configured(content).unwrap_err();
    }
}
