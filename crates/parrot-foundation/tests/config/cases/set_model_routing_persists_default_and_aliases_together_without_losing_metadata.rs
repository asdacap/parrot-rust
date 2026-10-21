use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config=context.configured("theme: dark\nmodel_aliases:\n  high_llm:\n    model_string: provider/old\n    usage: Customized usage\n    augment_system_prompt: Keep metadata.\n").unwrap();
    config
        .set_model_routing(
            "high_llm",
            &[("high_llm".into(), "provider/new/high".into())].into(),
        )
        .unwrap();
    let reloaded = context.load().unwrap();
    assert_eq!(reloaded.model, "high_llm");
    assert_eq!(
        reloaded.models.model_aliases["high_llm"].model_string,
        "provider/new/high"
    );
    assert_eq!(
        reloaded.models.model_aliases["high_llm"].usage,
        "Customized usage"
    );
    assert_eq!(
        reloaded.models.model_aliases["high_llm"]
            .augment_system_prompt
            .as_deref(),
        Some("Keep metadata.")
    );
    assert!(context.saved().contains("theme: dark"));
}
