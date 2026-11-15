use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture
        .configured("profiles:\n  plan:\n    prompt: Custom plan guidance\n    max_turns: 7\n")
        .unwrap();
    let profile = &configuration.runtime.profiles["plan"];
    assert_eq!(profile.prompt, "Custom plan guidance");
    assert_eq!(profile.max_turns, 7);
    assert!(profile.read_only);
    assert!(profile.sandbox_rules.is_empty());
}
