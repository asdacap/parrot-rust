use crate::support::Context;
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};

#[test]
fn source_assertions() {
    let context = Context::new();
    let root = context.directory.join("replacement");
    std::fs::create_dir(&root).unwrap();
    let configuration = context
        .configured(&format!(
            "sandbox_rules: !replace\n  - path: {}\n    rule: deny_write\n",
            root.display()
        ))
        .unwrap();
    assert_eq!(configuration.runtime.sandbox_rules.len(), 1);
    assert_eq!(
        configuration.runtime.sandbox_rules[0],
        SandboxRule {
            path: root.to_str().unwrap().into(),
            action: SandboxRuleAction::DenyWrite
        }
    );
    let empty = context.configured("sandbox_rules: !replace []\n").unwrap();
    assert!(empty.runtime.sandbox_rules.is_empty());
}
