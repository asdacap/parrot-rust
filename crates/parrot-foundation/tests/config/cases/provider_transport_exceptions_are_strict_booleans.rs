use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    for option in [
        "allow_insecure_localhost",
        "allow_insecure_remote",
        "allow_invalid_tls_certificate",
    ] {
        let fixture = ConfigFixture::new();
        for expected in [true, false] {
            let configuration = fixture
                .configured(&format!(
                    "providers:\n  custom:\n    {option}: {expected}\n"
                ))
                .unwrap();
            let provider = &configuration.models.providers["custom"];
            let value = match option {
                "allow_insecure_localhost" => provider.allow_insecure_localhost,
                "allow_insecure_remote" => provider.allow_insecure_remote,
                "allow_invalid_tls_certificate" => provider.allow_invalid_tls_certificate,
                _ => unreachable!(),
            };
            assert_eq!(value, expected);
        }
        let error = fixture
            .configured(&format!("providers:\n  custom:\n    {option}: null\n"))
            .unwrap_err();
        assert_eq!(
            error.0,
            format!("providers.custom.{option} must be true or false")
        );
    }
}
