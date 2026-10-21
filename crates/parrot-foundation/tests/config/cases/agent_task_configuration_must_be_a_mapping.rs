use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["null", "5", "[]"] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!("agent_tasks: {value}\n"))
                .unwrap_err()
                .to_string(),
            "agent_tasks must be a mapping"
        );
    }
}
