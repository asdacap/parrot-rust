use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert_eq!(
        fixture
            .configured("agent_tasks:\n  unsupported: 1\n")
            .unwrap_err()
            .to_string(),
        "agent_tasks contains an unsupported key"
    );
}
