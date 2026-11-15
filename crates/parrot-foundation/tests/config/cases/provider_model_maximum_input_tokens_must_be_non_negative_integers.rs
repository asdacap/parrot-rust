use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for limit in ["-1", "1.5", "not-a-number"] {
        let fixture = ConfigFixture::new();
        let result = fixture.configured(&format!(
            "providers:\n  custom:\n    models:\n      model:\n        max_input_tokens: {limit}\n"
        ));
        let _: parrot_foundation::config::ConfigError = result.unwrap_err();
    }
}
