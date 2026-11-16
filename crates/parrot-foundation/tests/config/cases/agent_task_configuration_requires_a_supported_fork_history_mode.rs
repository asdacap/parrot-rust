use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for value in [
        "1", "null", "true", "false", "yes", "[]", "{}", "Parent", "full", "\"\"",
    ] {
        let fixture = ConfigFixture::new();
        let error = fixture
            .configured(&format!("agent_tasks:\n  fork_history_mode: {value}\n"))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "agent_tasks.fork_history_mode must be dependency, parent, or empty"
        );
    }
}
