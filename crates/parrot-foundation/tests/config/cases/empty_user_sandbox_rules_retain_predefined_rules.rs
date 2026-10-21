use crate::support::Context;
#[test]
fn source_assertions() {
    let mut context = Context::new();
    context.environment.clear();
    context.environment.insert(
        "HOME".into(),
        context.directory.join("home").to_str().unwrap().into(),
    );
    context.environment.insert(
        "XDG_CACHE_HOME".into(),
        context.directory.join("cache").to_str().unwrap().into(),
    );
    let config = context.configured("sandbox_rules: []\n").unwrap();
    assert_eq!(config.runtime.sandbox_rules.len(), 5);
    assert_eq!(config.runtime.sandbox_rules[0].path, "/tmp");
}
