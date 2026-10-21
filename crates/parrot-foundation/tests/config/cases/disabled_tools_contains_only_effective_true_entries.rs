use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let disabled = context
        .configured(
            "disabled_tools:\n  web_fetch: true\n  agent_spawn: false\n  future_tool: true\n",
        )
        .unwrap()
        .runtime
        .disabled_tools;
    assert_eq!(disabled.len(), 2);
    assert!(disabled.contains("web_fetch"));
    assert!(disabled.contains("future_tool"));
    assert!(!disabled.contains("agent_spawn"));
}
