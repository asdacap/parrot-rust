use crate::support::{ConfigFixture, transact};
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut config=fixture.configured("theme: dark\nmodel_aliases:\n  high_llm:\n    model_string: provider/old\n    usage: Customized usage\n    augment_system_prompt: Keep metadata.\n").unwrap();
    transact(&mut config, |transaction| {
        transaction.set_model_routing_with_limits(
            "high_llm",
            &[("high_llm".into(), "provider/new/high".into())].into(),
            None,
        )
    })
    .unwrap();
    let reloaded = fixture.load().unwrap();
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
    assert!(fixture.saved().contains("theme: dark"));
}
