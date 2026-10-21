use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["-1", "null", "bad", "1.5", "2147483648"] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!(
                    "providers:\n  custom:\n    stream_idle_timeout_ms: {value}\n"
                ))
                .unwrap_err()
                .0,
            "providers.custom.stream_idle_timeout_ms must be a non-negative integer"
        );
    }
}
