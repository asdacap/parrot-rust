use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for value in ["null", "[1]"] {
        let fixture = ConfigFixture::new();
        let error = fixture
            .configured(&format!("request_limits: {value}\n"))
            .unwrap_err();
        assert_eq!(error.0, "request_limits must be a mapping");
    }
}
