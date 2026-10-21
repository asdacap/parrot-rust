use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let file = context.directory.join("file");
    std::fs::write(&file, "").unwrap();
    let impossible = file.join("child");
    assert!(context.configured(&format!("sandbox_rules:\n  - path: {}\n    rule: allow_write\n    create_if_not_exist: true\n",impossible.display())).is_err());
}
