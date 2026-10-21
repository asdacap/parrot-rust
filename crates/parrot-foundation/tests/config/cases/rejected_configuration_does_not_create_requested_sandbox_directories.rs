use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let directory = context.directory.join("missing/target");
    assert!(context.configured(&format!("sandbox_rules:\n  - path: {}\n    rule: allow_write\n    create_if_not_exist: true\nprofiles:\n  query:\n    read_only: invalid\n",directory.display())).is_err());
    assert!(!directory.is_dir());
}
