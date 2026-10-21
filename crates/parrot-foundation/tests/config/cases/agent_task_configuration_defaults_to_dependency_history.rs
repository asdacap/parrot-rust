use crate::support::Context;
use parrot_foundation::config::runtime::AgentTaskForkHistoryMode;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert_eq!(
        context
            .configured("")
            .unwrap()
            .runtime
            .agent_tasks
            .fork_history_mode,
        AgentTaskForkHistoryMode::Dependency
    );
}
