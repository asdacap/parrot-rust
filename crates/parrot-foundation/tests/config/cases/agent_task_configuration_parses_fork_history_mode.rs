use crate::support::ConfigFixture;
use parrot_foundation::config::runtime::AgentTaskForkHistoryMode;
#[test]
fn source_assertions() {
    for (value, expected) in [
        ("dependency", AgentTaskForkHistoryMode::Dependency),
        ("parent", AgentTaskForkHistoryMode::Parent),
        ("empty", AgentTaskForkHistoryMode::Empty),
    ] {
        let fixture = ConfigFixture::new();
        assert_eq!(
            fixture
                .configured(&format!("agent_tasks:\n  fork_history_mode: {value}\n"))
                .unwrap()
                .runtime
                .agent_tasks
                .fork_history_mode,
            expected
        );
    }
}
