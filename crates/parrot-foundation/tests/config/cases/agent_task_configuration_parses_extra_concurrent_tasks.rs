use crate::support::Context;
use parrot_foundation::config::runtime::AgentTaskForkHistoryMode;

#[test]
fn defaults_and_overrides_preserve_other_settings() {
    for (yaml, expected) in [
        ("", 2),
        ("agent_tasks:\n  extra_concurrent_tasks: 0\n", 0),
        ("agent_tasks:\n  extra_concurrent_tasks: 7\n", 7),
        (
            "agent_tasks:\n  extra_concurrent_tasks: 2147483647\n",
            i32::MAX,
        ),
    ] {
        let context = Context::new();
        let tasks = context.configured(yaml).unwrap().runtime.agent_tasks;
        assert_eq!(tasks.extra_concurrent_tasks, expected);
        assert_eq!(tasks.maximum_attempts, 5);
        assert_eq!(tasks.maximum_response_repairs, 3);
        assert_eq!(
            tasks.fork_history_mode,
            AgentTaskForkHistoryMode::Dependency
        );
    }
}

#[test]
fn rejects_invalid_values_with_field_path() {
    for value in [
        "-1",
        "1.5",
        "true",
        "false",
        "null",
        "words",
        "'2'",
        "\"2\"",
        "!!str 2",
        "2147483648",
        "999999999999999999999999999999999999",
        "[]",
        "{}",
    ] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!(
                    "agent_tasks:\n  extra_concurrent_tasks: {value}\n"
                ))
                .unwrap_err()
                .to_string(),
            "agent_tasks.extra_concurrent_tasks must be a non-negative integer",
            "value: {value}"
        );
    }
}
