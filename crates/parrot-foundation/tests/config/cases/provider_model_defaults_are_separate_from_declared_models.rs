use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("providers:\n  custom:\n    model_defaults:\n      seeded:\n        name: Seeded\n        context: 128\n        max_output_tokens: 32\n        input_price: 0.000001\n        cached_input_price: 0.0000001\n        output_price: 0.000002\n        tools: true\n        reasoning: false\n        output: [text]\n    models:\n      declared:\n        name: Declared\n        context: 256\n        max_output_tokens: 64\n        tools: false\n        reasoning: true\n        output: [text]\n").unwrap();
    let provider = &configuration.models.providers["custom"];
    assert!(provider.model_defaults.contains_key("seeded"));
    assert_eq!(provider.model_defaults["seeded"].input_price, 0.000001);
    assert_eq!(
        provider.model_defaults["seeded"].cached_input_price,
        0.0000001
    );
    assert_eq!(provider.model_defaults["seeded"].output_price, 0.000002);
    assert!(!provider.model_defaults.contains_key("declared"));
    assert!(provider.models.contains_key("declared"));
    assert!(!provider.models.contains_key("seeded"));
}
