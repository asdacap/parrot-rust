use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for id in ["explorer", "review", "worker", "thinker", "custom"] {
        let fixture = ConfigFixture::new();
        assert!(
            fixture
                .configured(&format!("default_profile: {id}\n"))
                .is_err()
        );
    }
}
