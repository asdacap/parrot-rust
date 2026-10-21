use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["true", "false", "null", "true\n  fork_history_mode: parent"] {
        let context = Context::new();
        let error = context
            .configured(&format!("agent_tasks:\n  fork_parent_history: {value}\n"))
            .unwrap_err();
        assert_eq!(
            error.0,
            "agent_tasks.fork_parent_history has been replaced by agent_tasks.fork_history_mode; use parent for true, empty for false, or dependency for the new default behavior"
        );
    }
}
