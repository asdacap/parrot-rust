use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    for key in ["image_bytes_per_tool_cycle", "provider_request_bytes"] {
        for value in ["0", "-1", "1.5", "2147483648", "null", "[1]"] {
            let fixture = ConfigFixture::new();
            let error = fixture
                .configured(&format!("request_limits:\n  {key}: {value}\n"))
                .unwrap_err();
            assert_eq!(
                error.0,
                format!("request_limits.{key} must be a positive integer"),
                "{key}={value}"
            );
        }
    }
}
