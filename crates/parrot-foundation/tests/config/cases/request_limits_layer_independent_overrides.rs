use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    for (key, value) in [
        ("image_bytes_per_tool_cycle", 1),
        ("image_bytes_per_read", 2),
        ("provider_request_bytes", i32::MAX),
    ] {
        let fixture = ConfigFixture::new();
        let defaults = fixture.configured("").unwrap().runtime.request_limits;
        let configured = fixture
            .configured(&format!("request_limits:\n  {key}: {value}\n"))
            .unwrap()
            .runtime
            .request_limits;
        let mut expected = defaults;
        match key {
            "image_bytes_per_tool_cycle" => expected.image_bytes_per_tool_cycle = value,
            "image_bytes_per_read" => expected.image_bytes_per_read = value,
            _ => expected.provider_request_bytes = value,
        }
        assert_eq!(configured, expected, "{key}={value}");
    }
}
