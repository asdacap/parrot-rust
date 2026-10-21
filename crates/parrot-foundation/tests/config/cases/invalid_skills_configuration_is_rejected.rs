use crate::support::Context;
#[test]
fn source_assertions() {
    for (yaml, message) in [
        ("skills: false", "skills must be a mapping"),
        (
            "skills:\n  enabled: yes",
            "skills.enabled must be true or false",
        ),
        (
            "skills:\n  entries: {}",
            "skills.entries must be a sequence",
        ),
        (
            "skills:\n  entries:\n    - path: relative/SKILL.md\n      enabled: true",
            "skills.entries[0].path must be an absolute path",
        ),
        (
            "skills:\n  entries:\n    - path: /tmp/SKILL.md",
            "skills.entries[0].enabled must be true or false",
        ),
        (
            "skills:\n  entries:\n    - path: /tmp/SKILL.md\n      enabled: true\n      name: forbidden",
            "skills.entries[0] contains an unsupported key",
        ),
    ] {
        let context = Context::new();
        let error = context.configured(&format!("{yaml}\n")).unwrap_err();
        assert_eq!(error.0, message);
    }
}
