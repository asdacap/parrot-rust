use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for (override_yaml, message) in [
        (
            "description: ''",
            "tools.read.description must be a non-empty string",
        ),
        (
            "description: null",
            "tools.read.description must be a non-empty string",
        ),
        ("parameters: []", "tools.read.parameters must be a mapping"),
        (
            "unsupported: value",
            "tools.read contains an unsupported key",
        ),
    ] {
        let fixture = ConfigFixture::new();
        let error = fixture
            .configured(&format!("tools:\n  read:\n    {override_yaml}\n"))
            .unwrap_err();
        assert_eq!(error.0, message);
    }
}
