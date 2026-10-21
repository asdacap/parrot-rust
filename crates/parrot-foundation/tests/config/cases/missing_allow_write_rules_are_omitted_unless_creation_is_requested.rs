use crate::support::Context;
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};

#[test]
fn source_assertions() {
    let context = Context::new();
    let missing = context.directory.join("missing/target");
    let configuration = context
        .configured(&format!(
            "sandbox_rules:\n  - path: {}\n    rule: allow_write\n",
            missing.display()
        ))
        .unwrap();
    assert_eq!(configuration.runtime.sandbox_rules.len(), 5);
    assert!(!configuration.runtime.sandbox_rules.contains(&SandboxRule {
        path: missing.to_str().unwrap().into(),
        action: SandboxRuleAction::AllowWrite
    }));
    assert!(!missing.is_dir());
}
