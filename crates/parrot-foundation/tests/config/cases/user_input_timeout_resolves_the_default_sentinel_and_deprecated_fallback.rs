use crate::support::Context;
#[test]
fn source_assertions() {
    for (yaml, expected) in [
        (None, 1200000),
        (Some("user_input_timeout_ms: 1250\n"), 1250),
        (Some("user_input_timeout_ms: -1\n"), -1),
        (Some("permission_request_timeout_ms: 2500\n"), 2500),
        (
            Some("permission_request_timeout_ms: 2500\nuser_input_timeout_ms: 1250\n"),
            1250,
        ),
    ] {
        let context = Context::new();
        let configuration = match yaml {
            None => context.load(),
            Some(yaml) => context.configured(yaml),
        }
        .unwrap();
        assert_eq!(configuration.runtime.user_input_timeout_ms, expected);
    }
}
