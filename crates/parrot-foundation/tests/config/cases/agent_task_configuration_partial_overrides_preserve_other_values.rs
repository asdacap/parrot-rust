use crate::support::Context;
use parrot_foundation::config::runtime::AgentTaskForkHistoryMode;
#[test]
fn source_assertions() {
    for (yaml, attempts, repairs, mode) in [
        (
            "agent_tasks:\n  maximum_attempts: 2\n",
            2,
            3,
            AgentTaskForkHistoryMode::Dependency,
        ),
        (
            "agent_tasks:\n  fork_history_mode: empty\n",
            5,
            3,
            AgentTaskForkHistoryMode::Empty,
        ),
        (
            "agent_tasks:\n  maximum_response_repairs: 1\n",
            5,
            1,
            AgentTaskForkHistoryMode::Dependency,
        ),
    ] {
        let context = Context::new();
        let tasks = context.configured(yaml).unwrap().runtime.agent_tasks;
        assert_eq!(tasks.maximum_attempts, attempts);
        assert_eq!(tasks.maximum_response_repairs, repairs);
        assert_eq!(tasks.fork_history_mode, mode);
    }
}
