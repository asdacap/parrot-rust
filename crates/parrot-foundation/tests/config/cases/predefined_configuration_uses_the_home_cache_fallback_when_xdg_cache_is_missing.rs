use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let mut fixture = ConfigFixture::new();
    let home = fixture.directory.join("home");
    fixture.environment.clear();
    fixture
        .environment
        .insert("HOME".into(), home.to_str().unwrap().into());
    let config = fixture.load().unwrap();
    let cache = home.join(".cache");
    assert!(
        config
            .runtime
            .sandbox_rules
            .iter()
            .any(|rule| rule.path == cache.to_str().unwrap())
    );
    assert!(cache.is_dir());
}
