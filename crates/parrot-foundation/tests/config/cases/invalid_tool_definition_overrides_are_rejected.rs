use crate::support::Context;
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
        let context = Context::new();
        let error = context
            .configured(&format!("tools:\n  read:\n    {override_yaml}\n"))
            .unwrap_err();
        assert_eq!(error.0, message);
    }
}
