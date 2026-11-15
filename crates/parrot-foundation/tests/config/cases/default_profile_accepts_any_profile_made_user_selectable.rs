use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture
        .configured("default_profile: worker\nprofiles:\n  worker:\n    is_user_selectable: true\n")
        .unwrap();
    assert_eq!(configuration.runtime.default_profile, "worker");
}
