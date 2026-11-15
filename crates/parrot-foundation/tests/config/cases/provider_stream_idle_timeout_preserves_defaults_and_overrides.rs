use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for provider_id in ["chatgpt", "openai", "custom"] {
        let fixture = ConfigFixture::new();
        let defaults = fixture
            .configured(&format!("providers:\n  {provider_id}: {{}}\n"))
            .unwrap();
        assert_eq!(
            defaults.models.providers[provider_id].stream_idle_timeout_ms,
            300000
        );
        let custom = fixture
            .configured(&format!(
                "providers:\n  {provider_id}:\n    stream_idle_timeout_ms: 2500\n"
            ))
            .unwrap();
        assert_eq!(
            custom.models.providers[provider_id].stream_idle_timeout_ms,
            2500
        );
        let disabled = fixture
            .configured(&format!(
                "providers:\n  {provider_id}:\n    stream_idle_timeout_ms: 0\n"
            ))
            .unwrap();
        assert_eq!(
            disabled.models.providers[provider_id].stream_idle_timeout_ms,
            0
        );
    }
}
