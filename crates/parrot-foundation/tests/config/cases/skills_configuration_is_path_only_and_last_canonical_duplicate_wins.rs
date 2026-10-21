use crate::support::Context;
use parrot_domain::values::config::SkillConfigEntry;
#[test]
fn source_assertions() {
    let context = Context::new();
    let first = context.directory.join("skills/../skill/SKILL.md");
    let canonical = context
        .directory
        .canonicalize()
        .unwrap()
        .join("skill/SKILL.md");
    let config=context.configured(&format!("skills:\n  enabled: true\n  entries:\n    - path: '{}'\n      enabled: false\n    - path: '{}'\n      enabled: true\n",first.display(),canonical.display())).unwrap();
    assert!(config.runtime.skills.enabled);
    assert_eq!(
        config.runtime.skills.entries,
        vec![SkillConfigEntry::new(canonical.to_str().unwrap(), true)]
    );
    assert!(
        config
            .runtime
            .skills
            .is_enabled(
                canonical.to_str().unwrap(),
                context.directory.to_str().unwrap(),
                parrot_domain::values::config::PathPlatform::Unix
            )
            .unwrap()
    );
}
