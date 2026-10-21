use crate::support::Context;
use parrot_foundation::config::models::ModelPresetConfig;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config=context.configured("theme: dark\nmodel_presets:\n  preserved:\n    model: provider/old\n    model_aliases: {}\n  work:\n    model: provider/old\n    model_aliases:\n      high_llm: provider/old\n").unwrap();
    config
        .set_model_preset(
            "work",
            &ModelPresetConfig {
                model: "high_llm".into(),
                model_aliases: [
                    ("custom_llm".into(), String::new()),
                    ("high_llm".into(), "provider/new/high".into()),
                ]
                .into(),
                context_limits: None,
            },
        )
        .unwrap();
    let reloaded = context.load().unwrap();
    assert_eq!(reloaded.models.model_presets["work"].model, "high_llm");
    assert!(reloaded.models.model_presets["work"].model_aliases["custom_llm"].is_empty());
    assert_eq!(
        reloaded.models.model_presets["preserved"].model,
        "provider/old"
    );
    assert!(context.saved().contains("theme: dark"));
}
