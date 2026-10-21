use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    assert!(!context.load().unwrap().runtime.agent_send_to_parent);
    assert!(
        context
            .configured("agent_send_to_parent: true\n")
            .unwrap()
            .runtime
            .agent_send_to_parent
    );
    let error = context
        .configured("agent_send_to_parent: yes\n")
        .unwrap_err();
    assert_eq!(error.0, "agent_send_to_parent must be true or false");
}
