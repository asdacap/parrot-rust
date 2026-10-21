use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "user_input_timeout_ms: 0\n",
        "user_input_timeout_ms: -2\n",
        "user_input_timeout_ms: 1.5\n",
        "user_input_timeout_ms: true\n",
        "user_input_timeout_ms: null\n",
        "user_input_timeout_ms: 2147483648\n",
        "permission_request_timeout_ms: 0\n",
    ] {
        let context = Context::new();
        let _: parrot_foundation::config::ConfigError = context.configured(content).unwrap_err();
    }
}
