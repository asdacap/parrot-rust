use crate::support::Context;
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};
#[test]
fn source_assertions() {
    let context = Context::new();
    let missing = context.directory.join("missing/target");
    let config = context
        .configured(&format!(
            "sandbox_rules:\n  - path: {}\n    rule: deny_read\n",
            missing.display()
        ))
        .unwrap();
    assert!(config.runtime.sandbox_rules.contains(&SandboxRule {
        path: missing.to_str().unwrap().into(),
        action: SandboxRuleAction::DenyRead
    }));
}
