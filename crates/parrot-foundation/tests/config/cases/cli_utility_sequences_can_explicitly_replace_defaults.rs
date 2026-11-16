use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture
        .configured("cli_utilities:\n  expected: !replace [custom]\n  optional: !replace []\n")
        .unwrap();
    assert_eq!(configuration.runtime.cli_utilities.expected, vec!["custom"]);
    assert!(configuration.runtime.cli_utilities.optional.is_empty());
}
