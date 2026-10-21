use crate::support::Context;
use parrot_foundation::config::models::{ModelAliasConfig, ModelAliasIconConfig};
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("model_aliases:\n  low_llm:\n    model_string: openai/gpt-5\n  xhigh_llm:\n    usage: Specialized strategic work\n  local:\n    model_string: ollama/qwen3\n    usage: Local implementation work\n    augment_system_prompt: Use local tools first.\n").unwrap();
    let aliases = &configuration.models.model_aliases;
    assert_eq!(aliases["low_llm"], ModelAliasConfig {
        model_string: "openai/gpt-5".into(),
        usage: "Explicit reversible mechanical or evidence work with failure-specific validation; never judgmental review. use fork none for this one please. It tend to confuse instruction also.".into(),
        augment_system_prompt: Some("Please refuse to decide or to determine fact that is not clearly stated in code or docs. Only provide evidence, do not infer or determine a fact.".into()),
        icon: Some(ModelAliasIconConfig { glyph: "◆".into(), color: "gray".into() }),
        context_limit: None,
    });
    assert_eq!(
        aliases["xhigh_llm"],
        ModelAliasConfig {
            model_string: String::new(),
            usage: "Specialized strategic work".into(),
            augment_system_prompt: None,
            icon: Some(ModelAliasIconConfig {
                glyph: "◆".into(),
                color: "red".into()
            }),
            context_limit: None
        }
    );
    assert_eq!(
        aliases["local"],
        ModelAliasConfig {
            model_string: "ollama/qwen3".into(),
            usage: "Local implementation work".into(),
            augment_system_prompt: Some("Use local tools first.".into()),
            icon: None,
            context_limit: None
        }
    );
}
