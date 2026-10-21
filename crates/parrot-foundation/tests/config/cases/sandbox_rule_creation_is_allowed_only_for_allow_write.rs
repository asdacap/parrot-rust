use crate::support::Context;
#[test]
fn source_assertions() {
    for action in ["allow_read", "deny_read", "deny_write"] {
        let context = Context::new();
        let path = context.directory.join("missing");
        let _: parrot_foundation::config::ConfigError = context
            .configured(&format!(
                "sandbox_rules:\n  - path: {}\n    rule: {action}\n    create_if_not_exist: true\n",
                path.display()
            ))
            .unwrap_err();
    }
}
