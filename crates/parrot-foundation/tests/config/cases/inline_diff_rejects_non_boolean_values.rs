use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let _: parrot_foundation::config::ConfigError =
        fixture.configured("inline_diff: yes\n").unwrap_err();
}
