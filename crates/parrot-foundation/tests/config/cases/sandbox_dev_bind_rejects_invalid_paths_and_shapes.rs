use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for value in [
        "/dev/dri",
        "[null]",
        "[{}]",
        "['']",
        "[relative/device]",
        "[/device]",
        "[/dev/../../tmp]",
    ] {
        let fixture = ConfigFixture::new();
        fixture.write(&format!("sandbox:\n  dev_bind: {value}\n"));
        let _: parrot_foundation::config::ConfigError = fixture.load().unwrap_err();
        let _: parrot_foundation::config::ConfigError = fixture
            .load()
            .and_then(|configuration| configuration.reload_model_configuration())
            .unwrap_err();
    }
}
