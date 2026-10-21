use crate::support::Context;
#[test]
fn source_assertions() {
    for provider in ["chatgpt", "openai", "custom"] {
        for (settings, timeout, retries) in [
            ("{}", 60000, 5),
            (
                "\n    header_timeout_ms: 2500\n    header_timeout_max_retries: 2",
                2500,
                2,
            ),
            (
                "\n    header_timeout_ms: 0\n    header_timeout_max_retries: 0",
                0,
                0,
            ),
        ] {
            let context = Context::new();
            let configuration = context
                .configured(&format!("providers:\n  {provider}: {settings}\n"))
                .unwrap();
            let actual = &configuration.models.providers[provider];
            assert_eq!(actual.header_timeout_ms, timeout);
            assert_eq!(actual.header_timeout_max_retries, retries);
        }
    }
}
