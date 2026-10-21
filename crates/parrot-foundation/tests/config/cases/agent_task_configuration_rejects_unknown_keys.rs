use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert_eq!(
        context
            .configured("agent_tasks:\n  unsupported: 1\n")
            .unwrap_err()
            .to_string(),
        "agent_tasks contains an unsupported key"
    );
}
