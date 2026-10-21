use crate::support::Context;
#[test]
fn source_assertions() {
    for yaml in [
        "model: !replace value\n",
        "profiles:\n  thinker:\n    allowed_tools: !replace []\n",
        "sandbox_rules:\n  - path: !replace /tmp\n    rule: deny_write\n",
    ] {
        let context = Context::new();
        assert!(context.configured(yaml).is_err());
    }
}
