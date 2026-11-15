use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for content in [
        "profiles:\n  build:\n    prompt: ''\n",
        "profiles:\n  build:\n    max_turns: 0\n",
        "profiles:\n  build:\n    max_turns: -1\n",
        "profiles:\n  build:\n    max_turns: not-a-number\n",
        "profiles:\n  build:\n    max_turns: 1.5\n",
    ] {
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(content).is_err());
    }
}
