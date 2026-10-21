use crate::support::Context;
#[test]
fn source_assertions() {
    for yaml in [
        "profiles:\n  query:\n    read_ony: true\n",
        "profiles:\n  build:\n    hard_rules: []\n",
        "sandbox_rules:\n  - path: /workspace\n    rules: allow_write\n",
    ] {
        let context = Context::new();
        assert!(context.configured(yaml).is_err());
    }
}
