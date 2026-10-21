use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("profiles:\n  build:\n    is_agent_selectable: true\n  worker:\n    is_user_selectable: true\n").unwrap();
    for id in ["build", "worker"] {
        assert!(configuration.runtime.profiles[id].is_user_selectable);
        assert!(configuration.runtime.profiles[id].is_agent_selectable);
    }
    assert!(configuration.runtime.profiles["query"].is_user_selectable);
    assert!(!configuration.runtime.profiles["query"].is_agent_selectable);
}
