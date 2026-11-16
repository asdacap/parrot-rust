use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let mut fixture = ConfigFixture::new();
    fixture.environment.clear();
    fixture.environment.insert(
        "HOME".into(),
        fixture.directory.join("home").to_str().unwrap().into(),
    );
    fixture.environment.insert(
        "XDG_CACHE_HOME".into(),
        fixture.directory.join("cache").to_str().unwrap().into(),
    );
    let config = fixture.configured("sandbox_rules: []\n").unwrap();
    assert_eq!(config.runtime.sandbox_rules.len(), 5);
    assert_eq!(config.runtime.sandbox_rules[0].path, "/tmp");
}
