use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for action in ["allow_read", "deny_read", "deny_write"] {
        let fixture = ConfigFixture::new();
        let path = fixture.directory.join("missing");
        let _: parrot_foundation::config::ConfigError = fixture
            .configured(&format!(
                "sandbox_rules:\n  - path: {}\n    rule: {action}\n    create_if_not_exist: true\n",
                path.display()
            ))
            .unwrap_err();
    }
}
