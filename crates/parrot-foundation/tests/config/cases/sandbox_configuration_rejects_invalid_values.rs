use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "sandbox:\n  enabled: yes\n",
        "sandbox:\n  enabled: 1\n",
        "sandbox:\n  unknown_key: true\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
