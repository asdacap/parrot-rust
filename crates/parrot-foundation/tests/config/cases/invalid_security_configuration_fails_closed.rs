use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    for content in [
        "sandbox_rules:\n  - path: relative\n    rule: allow_write\n",
        "sandbox_rules:\n  - path: /workspace\n    rule: unknown\n",
    ] {
        let _: parrot_foundation::config::ConfigError = context.configured(content).unwrap_err();
    }
    assert!(
        context
            .configured("profiles:\n  worker:\n    read_only: true\n")
            .unwrap()
            .runtime
            .profiles["worker"]
            .read_only
    );
    let _: parrot_foundation::config::ConfigError = context
        .configured("profiles:\n  query:\n    read_only: yes\n")
        .unwrap_err();
}
