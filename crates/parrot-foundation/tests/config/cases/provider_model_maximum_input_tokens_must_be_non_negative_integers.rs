use crate::support::Context;
#[test]
fn source_assertions() {
    for limit in ["-1", "1.5", "not-a-number"] {
        let context = Context::new();
        let result = context.configured(&format!(
            "providers:\n  custom:\n    models:\n      model:\n        max_input_tokens: {limit}\n"
        ));
        let _: parrot_foundation::config::ConfigError = result.unwrap_err();
    }
}
