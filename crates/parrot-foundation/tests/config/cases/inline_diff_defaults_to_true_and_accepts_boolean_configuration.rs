use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let missing = ConfigFixture::new();
    assert!(missing.load().unwrap().runtime.inline_diff);
    for (value, expected) in [("true", true), ("false", false)] {
        let fixture = ConfigFixture::new();
        assert_eq!(
            fixture
                .configured(&format!("inline_diff: {value}\n"))
                .unwrap()
                .runtime
                .inline_diff,
            expected
        );
    }
}
