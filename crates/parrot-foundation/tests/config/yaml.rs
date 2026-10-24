use parrot_foundation::config::node::{Kind, Node, Style};
use parrot_foundation::config::yaml::{
    compose, merge, parse_documents, parse_root, serialize, validate_replace_tags,
};

fn scalar_values(node: &Node) -> Vec<String> {
    match &node.kind {
        Kind::Scalar(value) => vec![value.clone().unwrap_or_default()],
        Kind::Sequence(items) => items.iter().flat_map(scalar_values).collect(),
        Kind::Mapping(entries) => entries
            .iter()
            .flat_map(|(key, value)| scalar_values(key).into_iter().chain(scalar_values(value)))
            .collect(),
    }
}

#[test]
fn raw_scalar_text_order_styles_tags_and_aliases_survive_parsing() {
    let root = parse_root(
        "first: 'true'\nsecond: \"00012\"\nthird: &items !replace [a, b]\nfourth: *items\n",
    )
    .unwrap();
    assert_eq!(root.get("first").unwrap().as_str(), Some("true"));
    assert_eq!(root.get("first").unwrap().style, Style::SingleQuoted);
    assert_eq!(root.get("second").unwrap().as_str(), Some("00012"));
    assert_eq!(root.get("second").unwrap().style, Style::DoubleQuoted);
    assert_eq!(root.get("third").unwrap().tag, "!replace");
    assert_eq!(
        root.get("third").unwrap().kind,
        root.get("fourth").unwrap().kind
    );
    assert_eq!(root.get("third").unwrap().anchor.as_deref(), Some("items"));
    assert_eq!(root.get("fourth").unwrap().alias.as_deref(), Some("items"));
    assert_eq!(root.as_mapping().unwrap()[0].0.as_str(), Some("first"));
}

#[test]
fn user_root_uses_only_first_mapping_document_and_rejects_duplicate_keys() {
    assert_eq!(parse_root("").unwrap(), Node::mapping(Vec::new()));
    assert_eq!(
        parse_root("scalar\n---\na: b").unwrap(),
        Node::mapping(Vec::new())
    );
    assert_eq!(
        parse_root("a: first\n---\na: second")
            .unwrap()
            .get("a")
            .unwrap()
            .as_str(),
        Some("first")
    );
    assert!(parse_documents("a: 1\n'a': 2").is_err());
    assert!(parse_documents("a: [").is_err());
}

#[test]
fn recursive_merge_appends_only_the_named_sequences() {
    let mut root = parse_root(
        "nested: {keep: yes, change: old}\nsandbox_rules: [base]\ncli_utilities: {expected: [a], optional: [b]}\nread_only_exec_command_prefixes: [rg]\nprofiles: {sample: {sandbox_rules: [initial], allowed_tools: [first]}}\nother: [old]\n",
    ).unwrap();
    let overrides = parse_root(
        "nested: {change: new, extra: unknown}\nsandbox_rules: [user]\ncli_utilities: {expected: [c], optional: !replace [d]}\nread_only_exec_command_prefixes: !replace []\nprofiles: {sample: {sandbox_rules: [later], allowed_tools: [second]}}\nother: [new]\n",
    ).unwrap();
    merge(&mut root, &overrides).unwrap();
    assert_eq!(
        root.get("nested").unwrap().get("keep").unwrap().as_str(),
        Some("yes")
    );
    assert_eq!(
        root.get("nested").unwrap().get("change").unwrap().as_str(),
        Some("new")
    );
    assert_eq!(
        scalar_values(root.get("sandbox_rules").unwrap()),
        ["base", "user"]
    );
    assert_eq!(
        scalar_values(root.get("cli_utilities").unwrap().get("expected").unwrap()),
        ["a", "c"]
    );
    assert_eq!(
        scalar_values(root.get("cli_utilities").unwrap().get("optional").unwrap()),
        ["d"]
    );
    assert_eq!(root.get("read_only_exec_command_prefixes").unwrap().tag, "");
    assert!(
        root.get("read_only_exec_command_prefixes")
            .unwrap()
            .as_sequence()
            .unwrap()
            .is_empty()
    );
    let sample = root.get("profiles").unwrap().get("sample").unwrap();
    assert_eq!(
        scalar_values(sample.get("sandbox_rules").unwrap()),
        ["initial", "later"]
    );
    assert_eq!(
        scalar_values(sample.get("allowed_tools").unwrap()),
        ["second"]
    );
    assert_eq!(scalar_values(root.get("other").unwrap()), ["new"]);
}

#[test]
fn replace_validation_recurses_through_unknown_mappings_and_sequence_items() {
    for (yaml, path) in [
        ("unknown: !replace [a]", "unknown"),
        ("sandbox_rules: !replace scalar", "sandbox_rules"),
        ("unknown: {nested: !replace []}", "unknown.nested"),
        (
            "sandbox_rules: [{unknown: !replace []}]",
            "sandbox_rules.unknown",
        ),
    ] {
        assert_eq!(
            validate_replace_tags(&parse_root(yaml).unwrap())
                .unwrap_err()
                .to_string(),
            format!("{path} may use !replace only on an append-enabled sequence")
        );
    }
    assert_eq!(
        validate_replace_tags(&parse_root("? [a, b]\n: value").unwrap())
            .unwrap_err()
            .to_string(),
        "configuration mapping keys must be strings"
    );
}

#[test]
fn compose_combines_only_disjoint_leaves_and_preserves_resource_order() {
    let sources = [
        (
            "prompts",
            Some(
                "system_prompts: {base: first}\nprompt_templates: {ordinary: {template: '{name}'}}",
            ),
        ),
        (
            "tasks",
            Some("system_prompts: {tasks: second}\ntools: {task: {description: task}}"),
        ),
        (
            "runtime",
            Some(
                "empty: {}\ntools: {sample: {parameters: {type: object}}}\nordered: [third, first, second]",
            ),
        ),
    ];
    let text = compose(&sources).unwrap();
    assert_eq!(text, compose(&sources).unwrap());
    assert!(text.ends_with("...\n"));
    let root = parse_root(&text).unwrap();
    assert_eq!(root.as_mapping().unwrap().len(), 5);
    assert_eq!(
        root.get("system_prompts")
            .unwrap()
            .as_mapping()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        scalar_values(root.get("ordered").unwrap()),
        ["third", "first", "second"]
    );
    for (first, second, path) in [
        ("model: first", "model: first", "model"),
        ("model: first", "model: second", "model"),
        (
            "system_prompts: {shared: first}",
            "system_prompts: {shared: second}",
            "system_prompts.shared",
        ),
        (
            "tools: {shared: {description: first}}",
            "tools: {shared: {description: second}}",
            "tools.shared.description",
        ),
        (
            "prompt_templates: {shared: {template: first}}",
            "prompt_templates: {shared: {template: second}}",
            "prompt_templates.shared.template",
        ),
        (
            "profiles: {shared: {prompt: first}}",
            "profiles: {shared: {prompt: second}}",
            "profiles.shared.prompt",
        ),
        (
            "arbitrary: {nested: {value: first}}",
            "arbitrary: {nested: {value: second}}",
            "arbitrary.nested.value",
        ),
        ("arbitrary: [first]", "arbitrary: [second]", "arbitrary"),
        ("arbitrary: []", "arbitrary: []", "arbitrary"),
        ("arbitrary: {}", "arbitrary: scalar", "arbitrary"),
        ("arbitrary: scalar", "arbitrary: {}", "arbitrary"),
    ] {
        assert_eq!(
            compose(&[("first.yaml", Some(first)), ("second.yaml", Some(second))])
                .unwrap_err()
                .to_string(),
            format!(
                "duplicate predefined configuration at {path} in \"first.yaml\" and \"second.yaml\""
            )
        );
    }
}

#[test]
fn compose_recursively_merges_arbitrary_mappings_in_resource_order() {
    let text = compose(&[
        (
            "first.yaml",
            Some("arbitrary: {nested: {first: one}, empty: {}}\nprofiles: {first: {prompt: initial}}"),
        ),
        (
            "second.yaml",
            Some("arbitrary: {nested: {second: [two, three]}, empty: {child: value}}\nprofiles: {first: {usage: later}, second: {prompt: another}}"),
        ),
    ])
    .unwrap();
    let root = parse_root(&text).unwrap();
    assert_eq!(
        scalar_values(&root),
        [
            "arbitrary",
            "nested",
            "first",
            "one",
            "second",
            "two",
            "three",
            "empty",
            "child",
            "value",
            "profiles",
            "first",
            "prompt",
            "initial",
            "usage",
            "later",
            "second",
            "prompt",
            "another",
        ]
    );
}

#[test]
fn compose_merges_disjoint_section_fields_and_duplicate_empty_mappings() {
    let text = compose(&[
        (
            "first.yaml",
            Some("prompt_templates: {shared: {template: first}, empty: {}}\ntools: {empty: {}}"),
        ),
        (
            "second.yaml",
            Some("prompt_templates: {shared: {allowed_arguments: []}, empty: {}}\ntools: {empty: {}}"),
        ),
    ])
    .unwrap();
    assert_eq!(
        text,
        "prompt_templates: {shared: {template: first, allowed_arguments: []}, empty: {}}\ntools: {empty: {}}\n...\n"
    );
}

#[test]
fn compose_leaves_section_schema_validation_to_configuration() {
    for section in ["prompt_templates", "system_prompts", "tools"] {
        let content = format!("{section}: []");
        assert_eq!(
            compose(&[("source.yaml", Some(&content))]).unwrap(),
            format!("{content}\n...\n")
        );
    }
}

#[test]
fn compose_invalid_sources_report_resource_and_reason() {
    for (content, reason) in [
        ("model: [", "invalid predefined configuration YAML"),
        (
            "model: one\n---\nmodel: two",
            "predefined configuration must contain exactly one mapping document",
        ),
        (
            "- item",
            "predefined configuration must contain exactly one mapping document",
        ),
        (
            "scalar",
            "predefined configuration must contain exactly one mapping document",
        ),
        (
            "",
            "predefined configuration must contain exactly one mapping document",
        ),
        (
            "tools:\n  ? [one, two]\n  : value",
            "configuration mapping keys at tools must be strings",
        ),
    ] {
        assert_eq!(
            compose(&[("invalid.yaml", Some(content))])
                .unwrap_err()
                .to_string(),
            format!("invalid.yaml: {reason}")
        );
    }
    assert_eq!(
        compose(&[("missing.yaml", None)]).unwrap_err().to_string(),
        "predefined configuration resource \"missing.yaml\" was not embedded in this build"
    );
}

#[test]
fn serialization_round_trips_quoted_values_unknown_keys_and_nested_collections() {
    let root = parse_root(
        "theme: dark\nquoted: 'true'\nopaque: {leading: '001', explicit: \"false\", nulltext: null, empty: ''}\nlist: [{a: b, nested: [one, two]}, [], {}]\ntext: |-\n  First {literal}\n  Second line\n",
    ).unwrap();
    let text = serialize(&root);
    let restored = parse_root(&text).unwrap();
    assert_eq!(scalar_values(&root), scalar_values(&restored));
    assert!(!text.ends_with("...\n"));
    assert_eq!(text, serialize(&root));
}

#[test]
fn multiline_scalar_styles_round_trip_without_folding_away_newlines() {
    for style in [
        Style::Plain,
        Style::SingleQuoted,
        Style::Literal,
        Style::Folded,
        Style::Any,
    ] {
        for value in ["first\nsecond", "first\n\nsecond", "first\n second"] {
            let mut scalar = Node::scalar(value);
            scalar.style = style;
            let root = Node::mapping(vec![(Node::scalar("value"), scalar)]);
            let text = serialize(&root);
            let parsed = parse_root(&text).unwrap();
            assert_eq!(
                parsed.get("value").unwrap().as_str(),
                Some(value),
                "{style:?}: {text:?}"
            );
        }
    }
}

#[test]
fn anchor_writeback_matches_independent_source_after_original_slot_is_replaced() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/anchor-writeback-input.json")).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/anchor-writeback-oracle.json")).unwrap();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tmp")
        .join(format!("t11-anchor-writeback-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let actual = super::fixtures::execute(&inputs, &root).unwrap();
    assert_eq!(actual, expected);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn replaced_anchor_definitions_relocate_by_identity_not_reused_name() {
    let input = "model: &chosen provider/old\nfirst: *chosen\nreplacement: &chosen provider/new\nsecond: *chosen\n";
    let mut root = parse_root(input).unwrap();
    assert_eq!(
        root,
        parse_root(input).unwrap(),
        "value equality must not depend on allocation identity"
    );
    let first_identity = root.get("first").unwrap().anchor_identity;
    let second_identity = root.get("second").unwrap().anchor_identity;
    assert!(first_identity.is_some());
    assert_ne!(first_identity, second_identity);
    root.set("model", Node::scalar("provider/latest"));
    let saved = serialize(&root);
    assert_eq!(
        saved,
        "model: provider/latest\nfirst: &chosen provider/old\nreplacement: &chosen provider/new\nsecond: *chosen\n"
    );
    let reparsed = parse_root(&saved).unwrap();
    assert_eq!(
        reparsed.get("first").unwrap().as_str(),
        Some("provider/old")
    );
    assert_eq!(
        reparsed.get("second").unwrap().as_str(),
        Some("provider/new")
    );
}

#[test]
fn shared_alias_mutations_match_independent_source_configuration_operations() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/anchor-mutation-input.json")).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/anchor-mutation-oracle.json")).unwrap();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tmp")
        .join(format!("t11-anchor-mutation-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let actual = super::fixtures::execute(&inputs, &root).unwrap();
    assert_eq!(actual, expected);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn distinct_field_mutations_through_aliases_preserve_preceding_edits() {
    for nested in [false, true] {
        let yaml = "a: &shared {first: old, second: old}\nb: *shared\n";
        let mut root = parse_root(&if nested {
            format!("outer:\n  {}", yaml.replace('\n', "\n  "))
        } else {
            yaml.to_owned()
        })
        .unwrap();
        let fields = if nested {
            root.get_mut("outer").unwrap()
        } else {
            &mut root
        };
        fields
            .get_mut("a")
            .unwrap()
            .set("first", Node::scalar("new-first"));
        fields
            .get_mut("b")
            .unwrap()
            .set("second", Node::scalar("new-second"));
        for key in ["a", "b"] {
            let value = fields.get(key).unwrap();
            assert_eq!(value.get("first").unwrap().as_str(), Some("new-first"));
            assert_eq!(value.get("second").unwrap().as_str(), Some("new-second"));
        }
        let saved = serialize(&root);
        let reloaded = parse_root(&saved).unwrap();
        let fields = if nested {
            reloaded.get("outer").unwrap()
        } else {
            &reloaded
        };
        for key in ["a", "b"] {
            let value = fields.get(key).unwrap();
            assert_eq!(value.get("first").unwrap().as_str(), Some("new-first"));
            assert_eq!(value.get("second").unwrap().as_str(), Some("new-second"));
        }
    }
}

#[test]
fn skill_shared_sequences_match_independent_source_configuration_operations() {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/skill-shared-sequence-input.json")).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/skill-shared-sequence-oracle.json")).unwrap();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tmp")
        .join(format!(
            "t11-skill-shared-sequence-test-{}",
            std::process::id()
        ));
    std::fs::create_dir_all(&root).unwrap();
    let actual = super::fixtures::execute(&inputs, &root).unwrap();
    assert_eq!(actual, expected);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mutable_shared_sequences_preserve_identity_and_preceding_edits() {
    let mut root = parse_root("a: &entries [first]\nb: *entries\n").unwrap();
    let identity = root.get("a").unwrap().anchor_identity;
    root.get_mut("a")
        .unwrap()
        .as_sequence_mut()
        .unwrap()
        .push(Node::scalar("second"));
    let alias = root.get_mut("b").unwrap();
    let metadata = (
        alias.anchor_identity,
        alias.style,
        alias.tag.clone(),
        alias.alias.clone(),
    );
    alias.as_sequence_mut().unwrap().push(Node::scalar("third"));
    assert_eq!(
        (
            alias.anchor_identity,
            alias.style,
            alias.tag.clone(),
            alias.alias.clone()
        ),
        metadata
    );
    assert_eq!(alias.anchor_identity, identity);
    assert_eq!(
        serialize(&root),
        "a: &entries [first, second, third]\nb: *entries\n"
    );
}

const OBSOLETE_COMPOSER_CASES: [&str; 6] = [
    "compose-duplicate-2",
    "compose-duplicate-3",
    "compose-invalid-5",
    "compose-invalid-6",
    "compose-invalid-7",
    "compose-shipped",
];

#[test]
fn independent_dotnet_emitter_and_composer_fixtures_match_exact_bytes() {
    assert_source_emitter_and_composer_fixtures(|id| !OBSOLETE_COMPOSER_CASES.contains(&id), 22);
}

#[test]
#[ignore = "abdaf0f changed section composition; later prompt changes obsolete live assets versus source bytes"]
fn obsolete_dotnet_composer_fixtures_match_exact_bytes() {
    assert_source_emitter_and_composer_fixtures(|id| OBSOLETE_COMPOSER_CASES.contains(&id), 6);
}

fn assert_source_emitter_and_composer_fixtures(
    select_case: impl Fn(&str) -> bool,
    expected_count: usize,
) {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/inputcases.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let mut failures = Vec::new();
    let mut compared = 0;
    for input in inputs.as_array().unwrap() {
        if !select_case(input["id"].as_str().unwrap()) {
            continue;
        }
        let kind = input["kind"].as_str().unwrap();
        let actual = match kind {
            "emit" => {
                let node = if let Some(constructed) = input.get("constructed") {
                    Node::mapping(vec![(
                        Node::scalar("value"),
                        Node::scalar(constructed.as_str().unwrap()),
                    )])
                } else {
                    parse_documents(input["yaml"].as_str().unwrap())
                        .unwrap()
                        .remove(0)
                };
                Ok(serialize(&node))
            }
            "compose" => {
                let sources = input["sources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|source| (source["name"].as_str().unwrap(), source["content"].as_str()))
                    .collect::<Vec<_>>();
                compose(&sources)
            }
            "compose_shipped" => compose(&[
                (
                    "Config/predefined_config.system_prompts.yaml",
                    Some(include_str!(
                        "../../../../assets/config/predefined_config.system_prompts.yaml"
                    )),
                ),
                (
                    "Config/predefined_config.agent_tasks.yaml",
                    Some(include_str!(
                        "../../../../assets/config/predefined_config.agent_tasks.yaml"
                    )),
                ),
                (
                    "Config/predefined_config.profiles.yaml",
                    Some(include_str!(
                        "../../../../assets/config/predefined_config.profiles.yaml"
                    )),
                ),
                (
                    "Config/predefined_config.runtime.yaml",
                    Some(include_str!(
                        "../../../../assets/config/predefined_config.runtime.yaml"
                    )),
                ),
            ]),
            _ => continue,
        };
        compared += 1;
        let expected = &oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == input["id"])
            .unwrap()["output"];
        let (actual, expected) = match actual {
            Ok(value) => (
                value,
                expected["value"]
                    .as_str()
                    .unwrap_or("<expected error>")
                    .to_owned(),
            ),
            Err(error) => (
                error.to_string(),
                expected["error"]
                    .as_str()
                    .unwrap_or("<expected success>")
                    .to_owned(),
            ),
        };
        if actual != expected {
            let first = actual
                .bytes()
                .zip(expected.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(actual.len().min(expected.len()));
            let start = first.saturating_sub(50);
            let actual_excerpt =
                String::from_utf8_lossy(&actual.as_bytes()[start..actual.len().min(first + 150)]);
            let expected_excerpt = String::from_utf8_lossy(
                &expected.as_bytes()[start..expected.len().min(first + 150)],
            );
            failures.push(format!("{}: first byte mismatch {} (actual {} vs source {} bytes)\nactual: {:?}\nsource: {:?}", input["id"], first, actual.len(), expected.len(), actual_excerpt, expected_excerpt));
        }
    }
    assert_eq!(
        compared, expected_count,
        "fixture selection unexpectedly changed"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
