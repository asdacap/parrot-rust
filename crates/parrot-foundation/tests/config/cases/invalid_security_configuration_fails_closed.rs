use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    for content in [
        "sandbox_rules:\n  - path: relative\n    rule: allow_write\n",
        "sandbox_rules:\n  - path: /workspace\n    rule: unknown\n",
    ] {
        let _: parrot_foundation::config::ConfigError = fixture.configured(content).unwrap_err();
    }
    assert!(
        fixture
            .configured("profiles:\n  worker:\n    read_only: true\n")
            .unwrap()
            .runtime
            .profiles["worker"]
            .read_only
    );
    let _: parrot_foundation::config::ConfigError = fixture
        .configured("profiles:\n  query:\n    read_only: yes\n")
        .unwrap_err();
}
