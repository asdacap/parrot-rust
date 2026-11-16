use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let directory = fixture.directory.join("missing/target");
    assert!(fixture.configured(&format!("sandbox_rules:\n  - path: {}\n    rule: allow_write\n    create_if_not_exist: true\nprofiles:\n  query:\n    read_only: invalid\n",directory.display())).is_err());
    assert!(!directory.is_dir());
}
