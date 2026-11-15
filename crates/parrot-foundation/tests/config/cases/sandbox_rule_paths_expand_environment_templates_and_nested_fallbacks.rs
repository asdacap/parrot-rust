use crate::support::ConfigFixture;
use parrot_foundation::config::runtime::{SandboxRule, SandboxRuleAction};
#[test]
fn source_assertions() {
    let mut fixture = ConfigFixture::new();
    let home = fixture
        .directory
        .join("home")
        .to_string_lossy()
        .into_owned();
    let cache = fixture
        .directory
        .join("cache")
        .to_string_lossy()
        .into_owned();
    let root = fixture
        .directory
        .join("root")
        .to_string_lossy()
        .into_owned();
    fixture.environment = [
        ("HOME".to_owned(), home),
        ("CACHE".to_owned(), cache.clone()),
        ("ROOT".to_owned(), root.clone()),
    ]
    .into();
    let configuration = fixture.configured("sandbox_rules:\n  - path: '${ROOT}/first/${CACHE_NAME:-cache}'\n    rule: allow_write\n    create_if_not_exist: true\n  - path: '${CACHE:-${MISSING}/unused}'\n    rule: deny_write\nprofiles:\n  build:\n    sandbox_rules:\n      - path: '${ROOT}/profile'\n        rule: deny_read\n").unwrap();
    let rules = &configuration.runtime.sandbox_rules;
    assert_eq!(rules.len(), 7);
    assert_eq!(
        rules[rules.len() - 2],
        SandboxRule {
            path: format!("{root}/first/cache"),
            action: SandboxRuleAction::AllowWrite
        }
    );
    assert_eq!(
        rules[rules.len() - 1],
        SandboxRule {
            path: cache,
            action: SandboxRuleAction::DenyWrite
        }
    );
    assert!(
        configuration.runtime.profiles["build"]
            .sandbox_rules
            .contains(&SandboxRule {
                path: format!("{root}/profile"),
                action: SandboxRuleAction::DenyRead
            })
    );
}
