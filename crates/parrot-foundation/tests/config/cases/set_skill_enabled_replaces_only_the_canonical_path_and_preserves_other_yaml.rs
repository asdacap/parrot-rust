use crate::support::ConfigFixture;
use parrot_domain::values::config::PathPlatform;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let canonical = fixture.directory.join("skill/SKILL.md");
    let alternate = fixture.directory.join("skill/../skill/SKILL.md");
    let other = fixture.directory.join("other/SKILL.md");
    let mut configuration = fixture.configured(&format!("theme: dark\nskills:\n  entries:\n    - path: '{}'\n      enabled: false\n    - path: '{}'\n      enabled: false\n    - path: '{}'\n      enabled: false\n", alternate.display(), canonical.display(), other.display())).unwrap();
    configuration
        .set_skill_enabled(canonical.to_str().unwrap(), true)
        .unwrap();
    let persisted = fixture.saved();
    let reloaded = fixture.load().unwrap();
    assert!(persisted.contains("theme: dark"));
    assert_eq!(reloaded.runtime.skills.entries.len(), 2);
    let enabled = |path: &std::path::Path| {
        reloaded
            .runtime
            .skills
            .is_enabled(
                path.to_str().unwrap(),
                fixture.directory.to_str().unwrap(),
                PathPlatform::Unix,
            )
            .unwrap()
    };
    assert!(enabled(&canonical));
    assert!(!enabled(&other));
    assert_eq!(
        configuration.runtime.skills.enabled,
        reloaded.runtime.skills.enabled
    );
    assert_eq!(
        configuration.runtime.skills.entries,
        reloaded.runtime.skills.entries
    );
    assert!(!std::path::PathBuf::from(format!("{}.tmp", fixture.path.display())).is_file());
}
