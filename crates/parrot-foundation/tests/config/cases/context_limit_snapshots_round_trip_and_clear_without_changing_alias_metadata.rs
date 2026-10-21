use crate::support::Context;
use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::models::{ModelPresetConfig, ModelPresetContextLimits};
use std::collections::BTreeMap;

#[test]
fn source_assertions() {
    for empty in [false, true] {
        let context = Context::new();
        let mut configuration = context.configured("theme: dark\ncontext_limit: 100k\nmodel_aliases:\n  high_llm:\n    model_string: provider/old\n    usage: Keep usage\n    context_limit: 20%\nmodel_presets:\n  legacy:\n    model: provider/old\n    model_aliases: {}\n  empty:\n    model: provider/old\n    model_aliases: {}\n    context_limits: {}\n").unwrap();
        assert!(
            configuration.models.model_presets["legacy"]
                .context_limits
                .is_none()
        );
        assert!(
            configuration.models.model_presets["empty"]
                .context_limits
                .is_some()
        );
        let limits = if empty {
            ModelPresetContextLimits {
                default: None,
                model_aliases: BTreeMap::new(),
            }
        } else {
            configuration.capture_context_limits()
        };
        configuration
            .set_model_preset(
                "saved",
                &ModelPresetConfig {
                    model: "provider/old".into(),
                    model_aliases: BTreeMap::new(),
                    context_limits: Some(limits),
                },
            )
            .unwrap();
        configuration
            .set_context_limit(ContextSize::parse("50k").unwrap())
            .unwrap();
        let legacy = configuration.models.model_presets["legacy"].clone();
        configuration
            .set_model_routing_with_limits(
                &legacy.model,
                &legacy.model_aliases,
                legacy.context_limits.as_ref(),
            )
            .unwrap();
        assert_eq!(
            context.load().unwrap().models.context_limit,
            Some(ContextSize::parse("50k").unwrap())
        );
        assert_eq!(
            context.load().unwrap().models.model_aliases["high_llm"].context_limit,
            Some(ContextSize::parse("20%").unwrap())
        );
        let saved = context.load().unwrap().models.model_presets["saved"].clone();
        configuration
            .set_model_routing_with_limits(
                &saved.model,
                &saved.model_aliases,
                saved.context_limits.as_ref(),
            )
            .unwrap();
        let restored = context.load().unwrap();
        assert_eq!(
            restored.models.context_limit,
            if empty {
                None
            } else {
                Some(ContextSize::parse("100k").unwrap())
            }
        );
        assert_eq!(
            restored.models.model_aliases["high_llm"].context_limit,
            if empty {
                None
            } else {
                Some(ContextSize::parse("20%").unwrap())
            }
        );
        assert_eq!(
            restored.models.model_aliases["high_llm"].usage,
            "Keep usage"
        );
        assert!(context.saved().contains("theme: dark"));
    }
}
