use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for (override_yaml, message) in [
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

#[test]
fn omitted_or_empty_description_is_allowed() {
    let parameters = "    parameters:\n      type: object\n";
    for description in ["", "    description: ''\n", "    description: null\n"] {
        let fixture = ConfigFixture::new();
        let settings = fixture
            .configured(&format!("tools:\n  probe_tool:\n{description}{parameters}"))
            .unwrap();
        assert_eq!(settings.runtime.tools["probe_tool"].description, "");
    }
}
