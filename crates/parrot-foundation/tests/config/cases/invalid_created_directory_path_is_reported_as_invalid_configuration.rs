use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let invalid = format!("/{}", "x".repeat(5000));
    assert!(context.configured(&format!("sandbox_rules:\n  - path: '{invalid}'\n    rule: allow_write\n    create_if_not_exist: true\n")).is_err());
}
