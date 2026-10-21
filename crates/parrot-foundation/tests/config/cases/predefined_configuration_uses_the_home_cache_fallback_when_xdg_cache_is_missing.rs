use crate::support::Context;
#[test]
fn source_assertions() {
    let mut context = Context::new();
    let home = context.directory.join("home");
    context.environment.clear();
    context
        .environment
        .insert("HOME".into(), home.to_str().unwrap().into());
    let config = context.load().unwrap();
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
