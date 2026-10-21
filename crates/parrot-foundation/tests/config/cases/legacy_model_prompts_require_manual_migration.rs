use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "model_augment_system_prompts: {}",
        "model_augment_system_prompts: null",
        "model_augment_system_prompts:\n  provider/model: Legacy prompt\nmodel_profiles:\n  provider/model:\n    system_prompt: New prompt",
    ] {
        let context = Context::new();
        let message = context.configured(content).unwrap_err().to_string();
        assert!(message.contains("model_augment_system_prompts is no longer supported"));
        assert!(message.contains("model_profiles.<selector>.system_prompt"));
    }
}
