use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let file = fixture.directory.join("file");
    std::fs::write(&file, "").unwrap();
    let impossible = file.join("child");
    assert!(fixture.configured(&format!("sandbox_rules:\n  - path: {}\n    rule: allow_write\n    create_if_not_exist: true\n",impossible.display())).is_err());
}
