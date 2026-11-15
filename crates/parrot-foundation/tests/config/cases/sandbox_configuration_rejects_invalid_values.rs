use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for content in [
        "sandbox:\n  enabled: yes\n",
        "sandbox:\n  enabled: 1\n",
        "sandbox:\n  unknown_key: true\n",
    ] {
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(content).is_err());
    }
}
