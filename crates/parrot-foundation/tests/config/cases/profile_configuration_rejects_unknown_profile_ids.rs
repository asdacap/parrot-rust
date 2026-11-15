use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert_eq!(
        fixture
            .configured("profiles:\n  unknown: {}\n")
            .unwrap_err()
            .to_string(),
        "profiles.unknown is not supported"
    );
}
