use crate::support::Context;
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};

#[test]
fn source_assertions() {
    let context = Context::new();
    let workspace = context.directory.join("workspace");
    let private_workspace = workspace.join("private");
    std::fs::create_dir_all(&private_workspace).unwrap();
    let configuration = context.configured(&format!(
        "sandbox_rules:\n  - path: {}\n    rule: allow_write\n  - path: {}\n    rule: deny_read\nprofiles:\n  build:\n    read_only: false\n    sandbox_rules:\n      - path: /workspace/generated\n        rule: deny_write\n  plan:\n    sandbox_rules: []\n  query:\n    read_only: true\n",
        workspace.display(), private_workspace.display()
    )).unwrap();
    let rules = &configuration.runtime.sandbox_rules;
    assert_eq!(rules.len(), 7);
    assert_eq!(rules[0].path, "/tmp");
    assert_eq!(
        rules[rules.len() - 2],
        SandboxRule {
            path: workspace.to_str().unwrap().into(),
            action: SandboxRuleAction::AllowWrite
        }
    );
    assert_eq!(rules.last().unwrap().action, SandboxRuleAction::DenyRead);
    assert!(!configuration.runtime.profiles["build"].read_only);
    assert_eq!(
        configuration.runtime.profiles["build"].sandbox_rules[0].action,
        SandboxRuleAction::DenyWrite
    );
    assert!(configuration.runtime.profiles["plan"].read_only);
    assert!(configuration.runtime.profiles["query"].read_only);
}
