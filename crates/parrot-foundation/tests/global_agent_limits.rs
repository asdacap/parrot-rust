use std::collections::BTreeMap;

use parrot_foundation::config::{
    configuration::source_predefined_content,
    node::Node,
    runtime::{AgentLimitsConfig, RuntimeSettings},
    yaml,
};

fn root() -> Node {
    yaml::parse_root(&source_predefined_content().unwrap()).unwrap()
}

fn read(root: &Node) -> Result<RuntimeSettings, parrot_foundation::config::ConfigError> {
    let environment = [
        ("HOME", "/tmp/parity-19/home"),
        ("WORKDIR", "/tmp/parity-19/workspace"),
        ("XDG_CONFIG_HOME", "/tmp/parity-19/config"),
        ("XDG_STATE_HOME", "/tmp/parity-19/state"),
        ("XDG_DATA_HOME", "/tmp/parity-19/data"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect::<BTreeMap<_, _>>();
    RuntimeSettings::read(root, &Node::mapping(vec![]), &environment)
}

#[test]
fn shipped_global_defaults_and_explicit_overrides_serialize_independently() {
    let mut root = root();
    let loaded = read(&root).unwrap();
    assert_eq!(loaded.agent_limits, AgentLimitsConfig::default());
    assert!(loaded.agent_limits.disable_max_turns);
    assert!(loaded.agent_limits.disable_recursion_limit);
    for (turns, recursion) in [(false, true), (true, false), (false, false)] {
        root.set("disable_max_turns", Node::scalar(turns.to_string()));
        root.set(
            "disable_recursion_limit",
            Node::scalar(recursion.to_string()),
        );
        let configured = read(&root).unwrap();
        assert_eq!(configured.agent_limits.disable_max_turns, turns);
        assert_eq!(configured.agent_limits.disable_recursion_limit, recursion);
        assert_eq!(
            configured.snapshot()["agentLimits"],
            serde_json::json!({
                "disableMaxTurns": turns,
                "disableRecursionLimit": recursion,
            })
        );
    }
}

#[test]
fn packaged_profiles_have_latest_recursion_limits() {
    let settings = read(&root()).unwrap();
    for (name, expected) in [
        ("query", 3),
        ("explorer", 4),
        ("worker", 6),
        ("agent-task-payload", 6),
    ] {
        assert_eq!(settings.profiles[name].recursion_limit, expected, "{name}");
    }
}

#[test]
fn global_agent_limits_require_source_boolean_values() {
    for key in ["disable_max_turns", "disable_recursion_limit"] {
        for invalid in [
            Node::scalar("yes"),
            Node::scalar("True"),
            Node::scalar("null"),
            Node::mapping(vec![]),
        ] {
            let mut node = root();
            node.set(key, invalid);
            assert_eq!(
                read(&node).unwrap_err().to_string(),
                format!("{key} must be true or false")
            );
        }
        let mut node = root();
        node.remove(key);
        assert_eq!(
            read(&node).unwrap_err().to_string(),
            format!("{key} must be true or false")
        );
    }
}
