use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parrot_foundation::config::environment;
use parrot_foundation::config::node::{Node, Style};
use parrot_foundation::config::runtime::{
    AgentTaskForkHistoryMode, PROFILE_IDS, RuntimeSettings, SandboxRuleAction, read_skills,
    read_tool_definitions, schema_json,
};

fn map(entries: &[(&str, Node)]) -> Node {
    Node::mapping(
        entries
            .iter()
            .map(|(key, value)| (Node::scalar(*key), value.clone()))
            .collect(),
    )
}
fn scalar(value: &str) -> Node {
    let mut node = Node::scalar(value);
    node.style = Style::Plain;
    node
}
fn strings(values: &[&str]) -> Node {
    Node::sequence(values.iter().map(|value| scalar(value)).collect())
}
fn quoted(value: &str) -> Node {
    let mut value = scalar(value);
    value.style = Style::SingleQuoted;
    value
}

fn root() -> Node {
    let profiles = Node::mapping(
        PROFILE_IDS
            .iter()
            .map(|id| {
                (
                    scalar(id),
                    map(&[
                        ("prompt", scalar("prompt")),
                        ("usage", scalar("usage")),
                        ("allowed_tools", scalar("null")),
                        ("max_turns", scalar("10")),
                        ("recursion_limit", scalar("0")),
                        ("read_only", scalar("false")),
                        ("is_user_selectable", scalar("true")),
                        ("is_agent_selectable", scalar("false")),
                    ]),
                )
            })
            .collect(),
    );
    map(&[
        ("profiles", profiles),
        ("default_profile", scalar("build")),
        (
            "cli_utilities",
            map(&[
                ("expected", strings(&["rg"])),
                ("optional", strings(&["rg", "git"])),
            ]),
        ),
        ("read_only_exec_command_prefixes", strings(&["git status"])),
        (
            "request_limits",
            map(&[
                ("image_bytes_per_tool_cycle", scalar("16777216")),
                ("image_bytes_per_read", scalar("3145728")),
                ("provider_request_bytes", scalar("67108864")),
            ]),
        ),
        ("user_input_timeout_ms", scalar("120000")),
        ("live_buffer_rows", scalar("20")),
        (
            "compaction",
            map(&[
                ("trigger_percent", scalar("90")),
                ("target_percent", scalar("30")),
                ("maximum_input_tokens", scalar("60000")),
                ("summary_output_tokens", scalar("12000")),
            ]),
        ),
        (
            "agent_tasks",
            map(&[
                ("maximum_attempts", scalar("5")),
                ("maximum_response_repairs", scalar("3")),
                ("fork_history_mode", scalar("dependency")),
            ]),
        ),
        ("agent_send_to_parent", scalar("false")),
        ("disable_max_turns", scalar("true")),
        ("disable_recursion_limit", scalar("true")),
        ("tools", Node::mapping(Vec::new())),
    ])
}
fn read(node: &Node) -> Result<RuntimeSettings, parrot_foundation::config::ConfigError> {
    RuntimeSettings::read(node, &Node::mapping(Vec::new()), &BTreeMap::new())
}
fn error(node: &Node) -> String {
    read(node).unwrap_err().to_string()
}

#[test]
fn read_only_collections_expose_slice_access_and_independent_clones() {
    use parrot_foundation::config::runtime::ReadOnlyCollection;
    let values = ReadOnlyCollection::new(vec!["git status".to_owned(), "rg".to_owned()]);
    assert_eq!(values.len(), 2);
    assert!(!values.is_empty());
    assert_eq!(
        values.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["git status", "rg"]
    );
    assert_eq!(&values[..1], &["git status".to_owned()]);
    assert_eq!(values.clone(), values);
    let mut copied_values = values.as_slice().to_vec();
    copied_values.push("git reset".to_owned());
    assert_eq!(values.len(), 2);
    assert!(ReadOnlyCollection::<String>::new(Vec::new()).is_empty());
}

#[test]
fn runtime_read_only_collections_keep_snapshot_arrays_unchanged() {
    let settings = read(&root()).unwrap();
    fn require_read_only(_: &parrot_foundation::config::runtime::ReadOnlyCollection<String>) {}
    require_read_only(&settings.read_only_exec_command_prefixes);
    require_read_only(&settings.sandbox_dev_bind);
    assert_eq!(settings.read_only_exec_command_prefixes[0], "git status");
    assert!(settings.sandbox_dev_bind.is_empty());
    let snapshot = settings.snapshot();
    assert_eq!(
        snapshot["readOnlyExecCommandPrefixes"],
        serde_json::json!(["git status"])
    );
    assert_eq!(snapshot["sandboxDevBind"], serde_json::json!([]));
}

#[test]
fn environment_templates_expand_nested_fallbacks_without_reparsing_values() {
    let env = BTreeMap::from([
        ("ROOT".to_owned(), "/root/${UNCHANGED}".to_owned()),
        ("EMPTY".to_owned(), String::new()),
    ]);
    assert_eq!(
        environment::resolve("${ROOT}/${EMPTY:-${CACHE:-cache}}/λ", "field", &env).unwrap(),
        "/root/${UNCHANGED}/cache/λ"
    );
    assert_eq!(
        environment::resolve("${ROOT:-${MISSING}}", "field", &env).unwrap(),
        "/root/${UNCHANGED}"
    );
    assert_eq!(
        environment::resolve("${EMPTY:-}", "field", &env).unwrap(),
        ""
    );
    assert_eq!(
        environment::resolve("ordinary $value { untouched", "field", &env).unwrap(),
        "ordinary $value { untouched"
    );
}

#[test]
fn environment_templates_reject_invalid_syntax_even_in_dormant_branches() {
    let env = BTreeMap::from([("ROOT".to_owned(), "/root".to_owned())]);
    for (text, message) in [
        ("${}", "contains an invalid environment variable name"),
        ("${1}", "contains an invalid environment variable name"),
        ("${ROOT", "contains an unclosed environment template"),
        (
            "${ROOT:+bad}",
            "contains an unsupported environment template operator",
        ),
        (
            "${ROOT:-${BAD:+x}}",
            "contains an unsupported environment template operator",
        ),
        ("x}", "contains an unmatched '}'"),
        (
            "${MISSING}",
            "requires nonempty environment variable MISSING",
        ),
    ] {
        assert_eq!(
            environment::resolve(text, "field", &env)
                .unwrap_err()
                .to_string(),
            format!("field {message}")
        );
    }
    let mut text = String::from("end");
    for _ in 0..33 {
        text = format!("${{MISSING:-{text}}}");
    }
    assert_eq!(
        environment::resolve(&text, "field", &env)
            .unwrap_err()
            .to_string(),
        "field template nesting is too deep"
    );
}

#[test]
fn runtime_default_record_shapes_and_timeout_legacy_precedence_match_source() {
    let root = root();
    let settings = read(&root).unwrap();
    assert_eq!(settings.profiles.len(), 9);
    assert_eq!(settings.cli_utilities.optional, vec!["git"]);
    assert_eq!(
        settings.agent_tasks.fork_history_mode,
        AgentTaskForkHistoryMode::Dependency
    );
    assert!(settings.inline_diff && settings.sandbox_enabled && settings.skills.enabled);
    let user = map(&[("permission_request_timeout_ms", scalar("-1"))]);
    assert_eq!(
        RuntimeSettings::read(&root, &user, &BTreeMap::new())
            .unwrap()
            .user_input_timeout_ms,
        -1
    );
    let user = map(&[
        ("permission_request_timeout_ms", scalar("-1")),
        ("user_input_timeout_ms", scalar("2500")),
    ]);
    assert_eq!(
        RuntimeSettings::read(&root, &user, &BTreeMap::new())
            .unwrap()
            .user_input_timeout_ms,
        2500
    );
    assert_eq!(
        settings.snapshot()["agentTasks"]["forkHistoryMode"],
        "Dependency"
    );
}

#[test]
fn integer_settings_reject_signs_whitespace_zero_overflow_and_non_scalars() {
    for value in [
        "0",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1.5",
        "2147483648",
        "null",
        "١",
    ] {
        let mut node = root();
        node.get_mut("request_limits")
            .unwrap()
            .set("image_bytes_per_read", scalar(value));
        assert_eq!(
            error(&node),
            "request_limits.image_bytes_per_read must be a positive integer"
        );
    }
    let mut node = root();
    node.get_mut("request_limits")
        .unwrap()
        .set("provider_request_bytes", scalar("2147483647"));
    assert_eq!(
        read(&node).unwrap().request_limits.provider_request_bytes,
        i32::MAX
    );
    for value in ["0", "-2", "+1", " 1", "1 ", "null", "2147483648"] {
        let mut node = root();
        node.set("user_input_timeout_ms", scalar(value));
        assert_eq!(
            error(&node),
            "user_input_timeout_ms must be a positive integer or -1"
        );
    }
    let mut node = root();
    node.set("user_input_timeout_ms", scalar("-1"));
    assert_eq!(read(&node).unwrap().user_input_timeout_ms, -1);
}

#[test]
fn profile_selection_allowed_tools_and_ignored_status_follow_source_validation() {
    let mut node = root();
    let profile = node.get_mut("profiles").unwrap().get_mut("build").unwrap();
    profile.set("status", scalar("legacy ignored field"));
    profile.set("allowed_tools", strings(&["question", "answer"]));
    assert_eq!(
        read(&node).unwrap().profiles["build"].allowed_tools,
        Some(vec!["question".to_owned(), "answer".to_owned()])
    );
    node.get_mut("profiles")
        .unwrap()
        .get_mut("build")
        .unwrap()
        .set("allowed_tools", strings(&["read", "read"]));
    assert_eq!(
        error(&node),
        "profiles.build.allowed_tools must contain unique nonblank untrimmed tool names"
    );
    let mut node = root();
    node.get_mut("profiles")
        .unwrap()
        .get_mut("build")
        .unwrap()
        .set("is_user_selectable", scalar("false"));
    assert_eq!(
        error(&node),
        "default_profile must name a user-selectable profile"
    );
    let mut node = root();
    node.get_mut("profiles")
        .unwrap()
        .set("custom", Node::mapping(Vec::new()));
    assert_eq!(error(&node), "profiles.custom is not supported");
}

#[test]
fn utility_prefix_disabled_tool_and_boolean_boundaries_preserve_errors() {
    let mut node = root();
    node.get_mut("cli_utilities")
        .unwrap()
        .set("expected", strings(&["rg", "rg"]));
    assert_eq!(
        error(&node),
        "cli_utilities.expected must contain unique nonempty utility names without whitespace or path separators"
    );
    for value in [
        scalar("null"),
        scalar("  git"),
        scalar(""),
        Node::mapping(Vec::new()),
    ] {
        let mut node = root();
        node.set(
            "read_only_exec_command_prefixes",
            Node::sequence(vec![value]),
        );
        assert_eq!(
            error(&node),
            "read_only_exec_command_prefixes[0] must be a unique nonblank string without leading or trailing whitespace"
        );
    }
    let mut node = root();
    node.set(
        "read_only_exec_command_prefixes",
        Node::sequence(vec![quoted("null")]),
    );
    assert_eq!(
        read(&node)
            .unwrap()
            .read_only_exec_command_prefixes
            .as_slice(),
        &["null".to_owned()]
    );
    node.set(
        "disabled_tools",
        map(&[("read", quoted("true")), ("write", scalar("false"))]),
    );
    assert!(read(&node).unwrap().disabled_tools.contains("read"));
    node.set("inline_diff", scalar("True"));
    assert_eq!(error(&node), "inline_diff must be true or false");
}

#[test]
fn compaction_and_agent_task_options_validate_order_and_retired_setting() {
    let mut node = root();
    node.get_mut("compaction")
        .unwrap()
        .set("trigger_percent", scalar("100"));
    assert_eq!(
        error(&node),
        "compaction requires 1 <= target_percent < trigger_percent <= 99"
    );
    let mut node = root();
    node.get_mut("agent_tasks")
        .unwrap()
        .set("fork_parent_history", scalar("false"));
    assert_eq!(
        error(&node),
        "agent_tasks.fork_parent_history has been replaced by agent_tasks.fork_history_mode; use parent for true, empty for false, or dependency for the new default behavior"
    );
    for (value, expected) in [
        ("parent", AgentTaskForkHistoryMode::Parent),
        ("empty", AgentTaskForkHistoryMode::Empty),
    ] {
        let mut node = root();
        node.get_mut("agent_tasks")
            .unwrap()
            .set("fork_history_mode", scalar(value));
        assert_eq!(read(&node).unwrap().agent_tasks.fork_history_mode, expected);
    }
}

#[test]
fn sandbox_device_paths_are_lexical_and_restricted_to_dev() {
    let mut node = root();
    node.set(
        "sandbox",
        map(&[
            ("enabled", scalar("false")),
            ("dev_bind", strings(&["/dev/dri/../nvidia0", "/dev/"])),
        ]),
    );
    let settings = read(&node).unwrap();
    assert!(!settings.sandbox_enabled);
    assert_eq!(
        settings.sandbox_dev_bind.as_slice(),
        &["/dev/nvidia0".to_owned(), "/dev/".to_owned()]
    );
    for path in ["/devices", "/dev/../../etc", "dev/zero", "null", ""] {
        let mut node = root();
        node.set(
            "sandbox",
            map(&[("enabled", scalar("true")), ("dev_bind", strings(&[path]))]),
        );
        assert_eq!(
            error(&node),
            "sandbox.dev_bind[0] must be an absolute device path under /dev"
        );
    }
}

#[test]
fn sandbox_missing_write_rules_skip_or_defer_creation_and_read_denials_survive() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "parrot-runtime-config-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let path = directory.to_str().unwrap();
    let rule = |action: &str, create: Option<bool>| {
        let mut node = map(&[("path", scalar("${ROOT}/child")), ("rule", scalar(action))]);
        if let Some(create) = create {
            node.set(
                "create_if_not_exist",
                scalar(if create { "true" } else { "false" }),
            );
        }
        node
    };
    let mut node = root();
    node.set(
        "sandbox_rules",
        Node::sequence(vec![rule("allow_write", None), rule("deny_read", None)]),
    );
    let env = BTreeMap::from([("ROOT".to_owned(), path.to_owned())]);
    let settings = RuntimeSettings::read(&node, &Node::mapping(Vec::new()), &env).unwrap();
    assert_eq!(settings.sandbox_rules.len(), 1);
    assert_eq!(
        settings.sandbox_rules[0].action,
        SandboxRuleAction::DenyRead
    );
    assert!(!directory.exists());
    node.set(
        "sandbox_rules",
        Node::sequence(vec![rule("allow_write", Some(true))]),
    );
    let settings = RuntimeSettings::read(&node, &Node::mapping(Vec::new()), &env).unwrap();
    assert!(!directory.exists());
    settings.provision_directories().unwrap();
    assert!(directory.join("child").is_dir());
    std::fs::remove_dir_all(&directory).unwrap();
    node.set(
        "sandbox_rules",
        Node::sequence(vec![rule("deny_read", Some(true))]),
    );
    assert_eq!(
        RuntimeSettings::read(&node, &Node::mapping(Vec::new()), &env)
            .unwrap_err()
            .to_string(),
        "sandbox_rules.0.create_if_not_exist is supported only for allow_write rules"
    );
}

#[test]
fn image_upload_retains_defaults_and_rejects_source_url_and_bucket_matrix() {
    let upload = |endpoint: &str, bucket: &str| {
        map(&[("endpoint", scalar(endpoint)), ("bucket", scalar(bucket))])
    };
    let mut node = root();
    node.set(
        "image_upload",
        upload("https://minio.example.com:9000/base", "b.1-2"),
    );
    let configured = read(&node).unwrap().image_upload.unwrap();
    assert_eq!(configured.region, "us-east-1");
    assert_eq!(configured.key_prefix, "parrot/");
    for endpoint in [
        "http://host",
        "ftp://host",
        "not a url",
        "https://host/?x=1",
        "https://user@host",
        "https://host/#fragment",
        "https://",
        "https://host:bad",
    ] {
        let mut node = root();
        node.set("image_upload", upload(endpoint, "bkt"));
        assert_eq!(error(&node), "image_upload.endpoint must be an https URL");
    }
    for bucket in ["", "ab", "Bucket", "-bucket", "bucket-", "bad_bucket"] {
        let mut node = root();
        node.set("image_upload", upload("https://host", bucket));
        assert_eq!(
            error(&node),
            "image_upload.bucket must be a valid S3 bucket name"
        );
    }
    let mut node = root();
    let mut image = upload("https://host", "bkt");
    image.set("secret_key", scalar("not-a-secret"));
    node.set("image_upload", image);
    assert_eq!(error(&node), "image_upload contains an unsupported key");
}

#[test]
fn skills_are_path_only_trim_directory_separators_and_last_duplicate_wins() {
    let entry = |path: &str, enabled: bool| {
        map(&[
            ("path", scalar(path)),
            ("enabled", scalar(if enabled { "true" } else { "false" })),
        ])
    };
    let node = map(&[(
        "skills",
        map(&[(
            "entries",
            Node::sequence(vec![
                entry("/missing/dir/../skill/", false),
                entry("/missing/skill", true),
            ]),
        )]),
    )]);
    let skills = read_skills(&node).unwrap();
    assert!(skills.enabled);
    assert_eq!(skills.entries.len(), 1);
    assert_eq!(skills.entries[0].path, "/missing/skill");
    assert!(skills.entries[0].enabled);
    for (path, message) in [
        (
            "relative",
            "skills.entries[0].path must be an absolute path",
        ),
        ("", "skills.entries[0].path must be a non-empty string"),
    ] {
        let node = map(&[(
            "skills",
            map(&[("entries", Node::sequence(vec![entry(path, false)]))]),
        )]);
        assert_eq!(read_skills(&node).unwrap_err().to_string(), message);
    }
}

#[test]
fn independent_dotnet_uri_whitespace_is_accepted_without_rewriting_configured_values() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/uri-whitespace-input.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/uri-whitespace-oracle.json")).unwrap();
    for input in inputs.as_array().unwrap() {
        let id = input["id"].as_str().unwrap();
        let expected = oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap();
        let parsed =
            parrot_foundation::config::yaml::parse_root(input["yaml"].as_str().unwrap()).unwrap();
        let mut node = root();
        node.set("image_upload", parsed.get("image_upload").unwrap().clone());
        match read(&node) {
            Err(error) => assert_eq!(
                serde_json::Value::String(error.to_string()),
                expected["output"]["error"],
                "{id}"
            ),
            Ok(runtime) => assert_eq!(
                runtime.snapshot()["imageUpload"],
                expected["output"]["value"]["selected"]["imageUpload"],
                "{id}"
            ),
        }
    }
    assert_eq!(inputs.as_array().unwrap().len(), 5);
}

#[test]
fn independent_dotnet_uri_and_schema_matrix_matches() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/inputcases.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let mut compared = 0;
    for input in inputs.as_array().unwrap() {
        let id = input["id"].as_str().unwrap();
        if !id.starts_with("uri-") && !id.starts_with("schema-scalar-") {
            continue;
        }
        let expected = oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap();
        let parsed =
            parrot_foundation::config::yaml::parse_root(input["yaml"].as_str().unwrap()).unwrap();
        let mut node = root();
        let section = if id.starts_with("uri-") {
            "image_upload"
        } else {
            "tools"
        };
        node.set(section, parsed.get(section).unwrap().clone());
        match read(&node) {
            Err(error) => assert_eq!(
                serde_json::Value::String(error.to_string()),
                expected["output"]["error"],
                "{id}"
            ),
            Ok(runtime) => {
                let snapshot = runtime.snapshot();
                for path in input["paths"].as_array().unwrap() {
                    let path = path.as_str().unwrap();
                    let value = path.split('.').fold(&snapshot, |value, key| &value[key]);
                    assert_eq!(
                        value, &expected["output"]["value"]["selected"][path],
                        "{id}: {path}"
                    );
                }
            }
        }
        compared += 1;
    }
    assert_eq!(compared, 27);
}

#[test]
fn tool_schema_scalars_preserve_quotes_json_numeric_lexemes_and_unknown_data() {
    let node = map(&[
        ("number", scalar("1e100")),
        ("negativeZero", scalar("-0")),
        ("notNumber", scalar("01")),
        ("quoted", quoted("true")),
        ("boolean", scalar("True")),
        ("nil", scalar("~")),
    ]);
    assert_eq!(
        schema_json(&node, "schema").unwrap(),
        "{\"number\":1e100,\"negativeZero\":-0,\"notNumber\":\"01\",\"quoted\":\"true\",\"boolean\":true,\"nil\":null}"
    );
    assert_eq!(
        schema_json(&scalar(".nan"), "schema")
            .unwrap_err()
            .to_string(),
        "schema must be representable as JSON"
    );
    let parameters = map(&[(
        "properties",
        map(&[(
            "offset",
            map(&[
                ("maximum", scalar("1e100")),
                ("default", map(&[("description", quoted(""))])),
            ]),
        )]),
    )]);
    let root = map(&[(
        "tools",
        map(&[(
            "read",
            map(&[
                ("description", scalar("Read file")),
                ("parameters", parameters),
            ]),
        )]),
    )]);
    let tools = read_tool_definitions(&root).unwrap();
    assert!(tools["read"].parameters_json.contains("\"maximum\":1e100"));
    let parameters = map(&[(
        "properties",
        map(&[("offset", map(&[("description", quoted(""))]))]),
    )]);
    let root = map(&[(
        "tools",
        map(&[(
            "read",
            map(&[
                ("description", scalar("Read file")),
                ("parameters", parameters),
            ]),
        )]),
    )]);
    assert_eq!(
        read_tool_definitions(&root).unwrap_err().to_string(),
        "tools.read.parameters.properties.offset.description must be a non-empty scalar"
    );
}
