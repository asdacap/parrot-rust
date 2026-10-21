use crate::support::Context;
#[test]
fn source_assertions() {
    for (provider_id, line, expected) in [
        ("custom", "", true),
        ("custom", "    image_urls: true\n", true),
        ("custom", "    image_urls: false\n", false),
        ("chatgpt", "", true),
        ("chatgpt", "    image_urls: false\n", false),
    ] {
        let context = Context::new();
        let configured = context
            .configured(&format!(
                "providers:\n  {provider_id}:\n    header_timeout_ms: 1\n{line}"
            ))
            .unwrap();
        assert_eq!(
            configured.models.providers[provider_id].image_urls,
            expected
        );
        let error = context
            .configured(&format!(
                "providers:\n  {provider_id}:\n    image_urls: maybe\n"
            ))
            .unwrap_err();
        assert_eq!(
            error.0,
            format!("providers.{provider_id}.image_urls must be true or false")
        );
    }
}
