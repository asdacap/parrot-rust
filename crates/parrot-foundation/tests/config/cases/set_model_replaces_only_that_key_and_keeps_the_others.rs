use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut config = fixture.configured("model: old\ntheme: dark\n").unwrap();
    config.set_model("new").unwrap();
    let rewritten = fixture.saved();
    assert!(rewritten.contains("model: new"));
    assert!(rewritten.contains("theme: dark"));
    assert!(!rewritten.contains("old"));
}
