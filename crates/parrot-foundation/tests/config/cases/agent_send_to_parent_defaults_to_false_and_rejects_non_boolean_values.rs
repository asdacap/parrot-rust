use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert!(!fixture.load().unwrap().runtime.agent_send_to_parent);
    assert!(
        fixture
            .configured("agent_send_to_parent: true\n")
            .unwrap()
            .runtime
            .agent_send_to_parent
    );
    let error = fixture
        .configured("agent_send_to_parent: yes\n")
        .unwrap_err();
    assert_eq!(error.0, "agent_send_to_parent must be true or false");
}
