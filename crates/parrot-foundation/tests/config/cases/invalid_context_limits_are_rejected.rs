use crate::support::Context;

#[test]
fn source_assertions() {
    for yaml in [
        "context_limit: 0",
        "model_aliases:\n  high_llm:\n    context_limit: 1.5k",
        "model_presets:\n  bad:\n    model: p/m\n    model_aliases: {}\n    context_limits: []",
    ] {
        assert!(
            Context::new().configured(yaml).is_err(),
            "source InvalidDataException case {yaml}"
        );
    }
}
