use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut config = fixture
        .configured("cli_utilities:\n  expected: !replace [custom]\n")
        .unwrap();
    config.set_model("glm-5.2").unwrap();
    assert!(fixture.saved().contains("!replace"));
    assert_eq!(
        fixture.load().unwrap().runtime.cli_utilities.expected,
        vec!["custom"]
    );
}
