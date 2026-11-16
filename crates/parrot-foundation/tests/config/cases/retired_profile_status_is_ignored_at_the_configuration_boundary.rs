use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert!(
        fixture
            .configured("profiles:\n  query:\n    status: retired\n")
            .unwrap()
            .runtime
            .profiles["query"]
            .prompt
            .contains("query mode")
    );
}
