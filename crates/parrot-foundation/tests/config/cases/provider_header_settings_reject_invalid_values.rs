use crate::support::Context;
#[test]
fn source_assertions() {
    for (setting, value) in [
        ("header_timeout_ms", "-1"),
        ("header_timeout_max_retries", "-1"),
        ("header_timeout_ms", "null"),
        ("header_timeout_max_retries", "bad"),
    ] {
        let context = Context::new();
        let failure = context
            .configured(&format!("providers:\n  custom:\n    {setting}: {value}\n"))
            .unwrap_err();
        assert_eq!(
            failure.to_string(),
            format!("providers.custom.{setting} must be a non-negative integer")
        );
    }
}
