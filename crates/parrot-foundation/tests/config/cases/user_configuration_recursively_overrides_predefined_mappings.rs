use crate::support::Context;
use parrot_foundation::config::models::{ModelAliasConfig, ModelAliasIconConfig};
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};
#[test]
fn source_assertions() {
    let context = Context::new();
    let workspace = context.directory.join("workspace");
    let config=context.configured(&format!("model_aliases:\n  low_llm:\n    model_string: openai/gpt-5\nprofiles:\n  build:\n    sandbox_rules:\n      - path: {}\n        rule: allow_write\n        create_if_not_exist: true\n",workspace.display())).unwrap();
    assert_eq!(config.models.model_aliases["low_llm"],ModelAliasConfig{model_string:"openai/gpt-5".into(),usage:"Explicit reversible mechanical or evidence work with failure-specific validation; never judgmental review. use fork none for this one please. It tend to confuse instruction also.".into(),augment_system_prompt:Some("Please refuse to decide or to determine fact that is not clearly stated in code or docs. Only provide evidence, do not infer or determine a fact.".into()),icon:Some(ModelAliasIconConfig{glyph:"◆".into(),color:"gray".into()}),context_limit:None});
    assert!(!config.runtime.profiles["build"].read_only);
    assert_eq!(
        config.runtime.profiles["build"].sandbox_rules,
        vec![SandboxRule {
            path: workspace.to_str().unwrap().into(),
            action: SandboxRuleAction::AllowWrite
        }]
    );
    assert!(context.saved().contains("model_string: openai/gpt-5"));
}
