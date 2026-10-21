use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "profiles:\n  build:\n    prompt: ''\n",
        "profiles:\n  build:\n    max_turns: 0\n",
        "profiles:\n  build:\n    max_turns: -1\n",
        "profiles:\n  build:\n    max_turns: not-a-number\n",
        "profiles:\n  build:\n    max_turns: 1.5\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
