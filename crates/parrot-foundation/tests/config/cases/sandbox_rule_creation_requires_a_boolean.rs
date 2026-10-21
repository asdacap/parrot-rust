use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["yes", "null", "[]"] {
        let context = Context::new();
        let _: parrot_foundation::config::ConfigError = context.configured(&format!("sandbox_rules:\n  - path: /tmp\n    rule: allow_write\n    create_if_not_exist: {value}\n")).unwrap_err();
    }
}
