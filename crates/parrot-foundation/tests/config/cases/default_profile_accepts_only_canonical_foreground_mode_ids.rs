use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for id in ["general", "coordinator", "build", "plan", "query"] {
        let fixture = ConfigFixture::new();
        assert_eq!(
            fixture
                .configured(&format!("default_profile: {id}\n"))
                .unwrap()
                .runtime
                .default_profile,
            id
        );
    }
}
