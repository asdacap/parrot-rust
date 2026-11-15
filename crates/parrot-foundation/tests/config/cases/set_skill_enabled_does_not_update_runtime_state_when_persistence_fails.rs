use crate::support::ConfigFixture;
use parrot_domain::values::config::PathPlatform;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut config = fixture.load().unwrap();
    let skill = fixture.directory.join("skill/SKILL.md");
    std::fs::create_dir(&fixture.path).unwrap();
    assert!(
        config
            .set_skill_enabled(skill.to_str().unwrap(), false)
            .is_err()
    );
    assert!(
        config
            .runtime
            .skills
            .is_enabled(
                skill.to_str().unwrap(),
                fixture.directory.to_str().unwrap(),
                PathPlatform::Unix
            )
            .unwrap()
    );
    assert!(!std::path::PathBuf::from(format!("{}.tmp", fixture.path.display())).exists());
}
