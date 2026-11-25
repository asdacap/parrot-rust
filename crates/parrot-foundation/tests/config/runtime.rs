use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use parrot_core::CancellationToken;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};

use parrot_foundation::config::environment;
use parrot_foundation::config::node::{Node, Style};
use parrot_foundation::config::runtime::{
    AgentTaskForkHistoryMode, McpTransport, PROFILE_IDS, RuntimeSettings, SandboxRuleAction,
    read_skills, read_tool_definitions, schema_json,
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

struct Active;

impl Cancellation for Active {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
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
        ("agent_ask_timeout_ms", scalar("300000")),
        ("exec_command_yield_after_ms", scalar("10000")),
        ("live_buffer_rows", scalar("20")),
        (
            "compaction",
            map(&[
                ("trigger_percent", scalar("90")),
                ("target_percent", scalar("30")),
                ("maximum_input_tokens", scalar("60000")),
                ("maximum_output_tokens", scalar("100000")),
                ("summary_output_tokens", scalar("12000")),
            ]),
        ),
        (
            "agent_tasks",
            map(&[
                ("extra_concurrent_tasks", scalar("2")),
                ("maximum_attempts", scalar("5")),
                ("maximum_response_repairs", scalar("3")),
                ("fork_history_mode", scalar("dependency")),
            ]),
        ),
        ("agent_send_to_parent", scalar("false")),
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
fn exec_automatic_yield_time_requires_a_positive_integer() {
    for value in ["0", "-1", "1.5", "null", "true", "2147483648"] {
        let mut node = root();
        node.set("exec_command_yield_after_ms", scalar(value));
        assert_eq!(
            error(&node),
            "exec_command_yield_after_ms must be a positive integer"
        );
    }
}

#[test]
fn spill_limits_default_to_bundled_values_and_validate_custom_values() {
    let settings = read(&root()).unwrap();
    assert_eq!(settings.tool_output_spill_limit_bytes, 64 * 1024);
    assert_eq!(settings.process_output_spill_limit_bytes, 2 * 1024);
    for key in [
        "tool_output_spill_limit_bytes",
        "process_output_spill_limit_bytes",
    ] {
        for value in ["0", "-1", "1.5", "null", "true", "2147483648"] {
            let mut node = root();
            node.set(key, scalar(value));
            assert_eq!(error(&node), format!("{key} must be a positive integer"));
        }
    }
    let mut node = root();
    node.set("tool_output_spill_limit_bytes", scalar("8192"));
    node.set("process_output_spill_limit_bytes", scalar("4096"));
    let settings = read(&node).unwrap();
    assert_eq!(settings.tool_output_spill_limit_bytes, 8192);
    assert_eq!(settings.process_output_spill_limit_bytes, 4096);
    let snapshot = settings.snapshot();
    assert_eq!(snapshot["toolOutputSpillLimitBytes"], 8192);
    assert_eq!(snapshot["processOutputSpillLimitBytes"], 4096);
}

#[test]
fn runtime_default_record_shapes_and_timeout_legacy_precedence_match_source() {
    let root = root();
    let settings = read(&root).unwrap();
    assert_eq!(settings.profiles.len(), PROFILE_IDS.len());
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
fn output_schema_defaults_and_section_values_parse_into_snapshot() {
    let settings = read(&root()).unwrap();
    assert_eq!(settings.output_schema.maximum_response_repairs, 3);

    let mut node = root();
    node.set(
        "output_schema",
        map(&[("maximum_response_repairs", scalar("5"))]),
    );
    let settings = read(&node).unwrap();
    assert_eq!(settings.output_schema.maximum_response_repairs, 5);
    assert_eq!(
        settings.snapshot()["outputSchema"]["maximumResponseRepairs"],
        5
    );

    let mut node = root();
    node.set(
        "output_schema",
        map(&[
            ("maximum_response_repairs", scalar("5")),
            ("unknown", scalar("x")),
        ]),
    );
    assert_eq!(error(&node), "output_schema contains an unsupported key");

    let mut node = root();
    node.set("output_schema", scalar("true"));
    assert_eq!(error(&node), "output_schema must be a mapping");

    let mut node = root();
    node.set(
        "output_schema",
        map(&[("maximum_response_repairs", scalar("0"))]),
    );
    assert_eq!(
        error(&node),
        "output_schema.maximum_response_repairs must be a positive integer"
    );
}

#[test]
fn output_schema_repair_template_renders_error_and_schema_values() {
    let templates = crate::fixtures::fixture_templates("").unwrap();
    let spec = &templates["output-schema.repair"];
    assert_eq!(spec.engine, "scalar");
    assert_eq!(spec.required, ["error", "schema"]);
    let allowed = spec.allowed.iter().cloned().collect::<BTreeSet<_>>();
    let compiled =
        parrot_core::ScalarPromptTemplateEngine::compile(&spec.path, &spec.text, &allowed).unwrap();
    let arguments = [
        ("error".to_owned(), "missing property result".to_owned()),
        ("schema".to_owned(), r#"{"type":"object"}"#.to_owned()),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    assert_eq!(
        compiled
            .render(&arguments, CancellationToken::new(&Active))
            .unwrap(),
        "Your previous response does not satisfy the required JSON schema: missing property result \
         Reply again with only a single JSON document that satisfies this schema and nothing else. \
         The required schema: {\"type\":\"object\"}"
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
    for value in ["0", "-1", "+1", " 1", "1 ", "null", "2147483648"] {
        let mut node = root();
        node.set("agent_ask_timeout_ms", scalar(value));
        assert_eq!(
            error(&node),
            "agent_ask_timeout_ms must be a positive integer"
        );
    }
    let mut node = root();
    node.set("agent_ask_timeout_ms", scalar("1250"));
    assert_eq!(read(&node).unwrap().agent_ask_timeout_ms, 1250);
    let mut node = root();
    node.set("user_input_timeout_ms", scalar("-1"));
    assert_eq!(read(&node).unwrap().user_input_timeout_ms, -1);
}

#[test]
fn profile_selection_allowed_tools_and_ignored_status_follow_source_validation() {
    let mut node = root();
    let profile = node.get_mut("profiles").unwrap().get_mut("build").unwrap();
    profile.set("status", scalar("legacy ignored field"));
    profile.set("allowed_tools", strings(&["ask", "reply"]));
    assert_eq!(
        read(&node).unwrap().profiles["build"].allowed_tools,
        Some(vec!["ask".to_owned(), "reply".to_owned()])
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
fn uri_whitespace_is_accepted_without_rewriting_configured_values() {
    for endpoint in [
        "https://host ",
        " https://host",
        "\thttps://host\n",
        "https://host/path ",
    ] {
        let mut node = root();
        node.set(
            "image_upload",
            map(&[
                ("endpoint", scalar(endpoint)),
                ("bucket", scalar("valid-bucket")),
            ]),
        );
        assert_eq!(
            read(&node).unwrap().image_upload.unwrap().endpoint,
            endpoint
        );
    }
    let mut node = root();
    node.set(
        "image_upload",
        map(&[
            ("endpoint", scalar("https://bad\thost/")),
            ("bucket", scalar("valid-bucket")),
        ]),
    );
    assert_eq!(error(&node), "image_upload.endpoint must be an https URL");
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

#[test]
fn mcp_servers_accept_valid_stdio_and_http_entries_with_defaults() {
    let mut node = root();
    node.set(
        "mcp",
        map(&[
            (
                "fs",
                map(&[
                    ("transport", scalar("stdio")),
                    ("command", scalar("/usr/bin/mcp-fs")),
                    ("args", strings(&["--stdio"])),
                    ("env", map(&[("TOKEN", scalar("abc"))])),
                    ("cwd", scalar("/work")),
                    ("enabled", scalar("true")),
                    ("startup_timeout_ms", scalar("0")),
                ]),
            ),
            (
                "remote",
                map(&[
                    ("transport", scalar("http")),
                    ("url", scalar("https://mcp.example.test/rpc")),
                    ("headers", map(&[("X-Tenant", scalar("t"))])),
                    ("allow_insecure_localhost", scalar("true")),
                    ("call_timeout_ms", scalar("1")),
                ]),
            ),
        ]),
    );
    let mcp = read(&node).unwrap().mcp;
    let fs = mcp.get("fs").unwrap();
    assert_eq!(fs.transport, McpTransport::Stdio);
    assert_eq!(fs.command.as_deref(), Some("/usr/bin/mcp-fs"));
    assert_eq!(fs.args, vec!["--stdio"]);
    assert_eq!(fs.env.get("TOKEN").map(String::as_str), Some("abc"));
    assert_eq!(fs.cwd.as_deref(), Some("/work"));
    assert!(fs.enabled);
    assert_eq!(fs.startup_timeout_ms, 0);
    assert_eq!(fs.call_timeout_ms, 30000);
    let remote = mcp.get("remote").unwrap();
    assert_eq!(remote.transport, McpTransport::Http);
    assert_eq!(remote.url.as_deref(), Some("https://mcp.example.test/rpc"));
    assert_eq!(
        remote.headers.get("X-Tenant").map(String::as_str),
        Some("t")
    );
    assert!(remote.allow_insecure_localhost);
    assert_eq!(remote.call_timeout_ms, 1);
    assert_eq!(remote.startup_timeout_ms, 15000);
    assert_eq!(
        read(&node).unwrap().snapshot()["mcp"]["fs"]["transport"],
        "stdio"
    );
    assert!(read(&root()).unwrap().mcp.is_empty());
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("stdio")),
                ("command", scalar("/bin/true")),
            ]),
        )]),
    );
    assert!(!read(&node).unwrap().mcp["fs"].enabled);
}

#[test]
fn mcp_servers_reject_unsupported_transport_inconsistent_and_invalid_entries() {
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[("transport", scalar("stdio")), ("bogus", scalar("true"))]),
        )]),
    );
    assert_eq!(error(&node), "mcp.fs contains an unsupported key");
    let mut node = root();
    node.set("mcp", map(&[("fs", map(&[("transport", scalar("grpc"))]))]));
    assert_eq!(error(&node), "mcp.fs.transport must be 'stdio' or 'http'");
    let mut node = root();
    node.set(
        "mcp",
        map(&[("fs", map(&[("transport", scalar("stdio"))]))]),
    );
    assert_eq!(
        error(&node),
        "mcp.fs.command must be a nonblank executable path"
    );
    let mut node = root();
    node.set("mcp", map(&[("fs", map(&[("transport", scalar("http"))]))]));
    assert_eq!(error(&node), "mcp.fs.url must be a nonblank endpoint URL");
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("stdio")),
                ("command", scalar("/bin/true")),
                ("url", scalar("https://x.test")),
            ]),
        )]),
    );
    assert_eq!(
        error(&node),
        "mcp.fs.url does not apply to the stdio transport"
    );
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("http")),
                ("url", scalar("https://x.test")),
                ("args", strings(&["--stdio"])),
            ]),
        )]),
    );
    assert_eq!(
        error(&node),
        "mcp.fs.args does not apply to the http transport"
    );
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("stdio")),
                ("command", scalar("/bin/true")),
                ("enabled", scalar("yes")),
            ]),
        )]),
    );
    assert_eq!(error(&node), "mcp.fs.enabled must be true or false");
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("stdio")),
                ("command", scalar("/bin/true")),
                ("startup_timeout_ms", scalar("-1")),
            ]),
        )]),
    );
    assert_eq!(
        error(&node),
        "mcp.fs.startup_timeout_ms must be a non-negative integer"
    );
    let mut node = root();
    node.set(
        "mcp",
        map(&[(
            "fs",
            map(&[
                ("transport", scalar("stdio")),
                ("command", scalar("/bin/true")),
                ("call_timeout_ms", scalar("-1")),
            ]),
        )]),
    );
    assert_eq!(
        error(&node),
        "mcp.fs.call_timeout_ms must be a non-negative integer"
    );
}

#[test]
fn filter_defaults_apply_when_section_missing() {
    let settings = read(&root()).unwrap();
    assert_eq!(
        settings.filter,
        parrot_foundation::config::runtime::FilterConfig::default()
    );
}

#[test]
fn filter_parses_explicit_section_values_and_wire_snapshot() {
    let mut node = root();
    node.set(
        "filter",
        map(&[
            ("enabled", scalar("true")),
            ("max_unfiltered_token_window", scalar("65536")),
            ("filter_batch_size", scalar("8192")),
        ]),
    );
    let settings = read(&node).unwrap();
    assert!(settings.filter.enabled);
    assert_eq!(settings.filter.max_unfiltered_token_window, 65536);
    assert_eq!(settings.filter.filter_batch_size, 8192);
    let snapshot = settings.snapshot();
    assert_eq!(snapshot["filter"]["enabled"], serde_json::json!(true));
    assert_eq!(
        snapshot["filter"]["maxUnfilteredTokenWindow"],
        serde_json::json!(65536)
    );
    assert_eq!(
        snapshot["filter"]["filterBatchSize"],
        serde_json::json!(8192)
    );

    let mut node = root();
    node.set("filter", map(&[("enabled", scalar("true"))]));
    let filter = read(&node).unwrap().filter;
    assert!(filter.enabled);
    assert_eq!(filter.max_unfiltered_token_window, 32768);
    assert_eq!(filter.filter_batch_size, 16384);
}

#[test]
fn filter_rejects_non_positive_token_values_and_unknown_keys() {
    for key in ["max_unfiltered_token_window", "filter_batch_size"] {
        for value in ["0", "-1", "1.5", "true", "null"] {
            let mut node = root();
            node.set("filter", map(&[(key, scalar(value))]));
            assert!(
                read(&node).is_err(),
                "{key} {value} must be rejected as a positive integer"
            );
        }
    }
    let mut node = root();
    node.set("filter", map(&[("unknown", scalar("1"))]));
    assert_eq!(error(&node), "filter contains an unsupported key");
}
