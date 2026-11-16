use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for content in [
        "provider_model_alias_defaults: []\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: other/xhigh\n",
        "provider_model_alias_defaults:\n  local:\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: local/xhigh\n    extra: local/extra\n",
        "provider_model_alias_defaults:\n  ' local ':\n    low_llm: local/low\n    medium_llm: local/medium\n    high_llm: local/high\n    xhigh_llm: local/xhigh\n",
    ] {
        let fixture = ConfigFixture::new();
        let _: parrot_foundation::config::ConfigError = fixture.configured(content).unwrap_err();
    }
}
