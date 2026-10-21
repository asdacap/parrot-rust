use crate::support::Context;
use std::collections::BTreeMap;

#[test]
fn source_assertions() {
    let context = Context::new();
    let root = context.directory.join("${OTHER}");
    context.write("sandbox_rules:\n  - path: '${ROOT}'\n    rule: allow_write\n    create_if_not_exist: true\n");
    let environment = BTreeMap::from([
        (
            "HOME".into(),
            context.directory.join("home").to_str().unwrap().into(),
        ),
        (
            "XDG_CACHE_HOME".into(),
            context.directory.join("cache").to_str().unwrap().into(),
        ),
        ("ROOT".into(), root.to_str().unwrap().into()),
        ("OTHER".into(), "expanded".into()),
    ]);
    let configuration = crate::fixtures::fixture_load_with_environment(
        &context.path,
        &context.predefined,
        &environment,
    )
    .unwrap();
    assert_eq!(
        configuration.runtime.sandbox_rules.last().unwrap().path,
        root.to_str().unwrap()
    );
}
