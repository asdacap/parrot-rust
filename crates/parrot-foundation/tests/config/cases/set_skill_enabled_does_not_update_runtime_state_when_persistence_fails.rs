use crate::support::Context;
use parrot_domain::values::config::PathPlatform;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config = context.load().unwrap();
    let skill = context.directory.join("skill/SKILL.md");
    std::fs::create_dir(&context.path).unwrap();
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
                context.directory.to_str().unwrap(),
                PathPlatform::Unix
            )
            .unwrap()
    );
    assert!(!std::path::PathBuf::from(format!("{}.tmp", context.path.display())).exists());
}
