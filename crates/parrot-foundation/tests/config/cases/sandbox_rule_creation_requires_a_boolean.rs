use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for value in ["yes", "null", "[]"] {
        let fixture = ConfigFixture::new();
        let _: parrot_foundation::config::ConfigError = fixture.configured(&format!("sandbox_rules:\n  - path: /tmp\n    rule: allow_write\n    create_if_not_exist: {value}\n")).unwrap_err();
    }
}
