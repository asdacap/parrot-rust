use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert!(
        fixture
            .configured("profiles:\n  build:\n    is_user_agent: true\n")
            .is_err()
    );
}
