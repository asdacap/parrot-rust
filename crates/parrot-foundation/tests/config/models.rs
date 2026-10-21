use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::models::{
    ModelConfigFields, ModelPresetContextLimits, ModelSettings, apply_context_limits,
    read_context_limit, read_context_limits, validate_alias_name, validate_model_selector,
    validate_preset_name, validate_requested_selector, write_context_limits,
};
use parrot_foundation::config::node::{Node, Style};
use std::collections::BTreeMap;

fn map(entries: Vec<(&str, Node)>) -> Node {
    Node::mapping(
        entries
            .into_iter()
            .map(|(key, value)| (Node::scalar(key), value))
            .collect(),
    )
}
fn s(value: &str) -> Node {
    let mut node = Node::scalar(value);
    node.style = Style::Plain;
    node
}
fn root(key: &str, value: Node) -> Node {
    map(vec![
        ("provider_model_alias_defaults", map(vec![])),
        (key, value),
    ])
}
fn alias(fields: Vec<(&str, Node)>) -> Node {
    map(vec![("custom", map(fields))])
}
fn quoted(value: &str) -> Node {
    let mut node = s(value);
    node.style = Style::SingleQuoted;
    node
}
fn read_alias(fields: Vec<(&str, Node)>) -> ModelSettings {
    ModelSettings::read(&root("model_aliases", alias(fields))).unwrap()
}

#[test]
fn independent_model_snapshots_match_dotnet_oracle() {
    use parrot_foundation::config::yaml::{compose, merge, parse_root};
    use serde_json::{Value, json};
    let sources = [
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
    ];
    let defaults = parse_root(&compose(&sources).unwrap()).unwrap();
    let inputs: Value = serde_json::from_str(include_str!("fixtures/inputcases.json")).unwrap();
    let oracle: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let mut count = 0;
    for input in inputs.as_array().unwrap() {
        let id = input["id"].as_str().unwrap();
        if !(id.starts_with("icon-")
            || id.starts_with("context-limit-")
            || matches!(
                id,
                "defaults"
                    | "recursive-override"
                    | "profile-resolution"
                    | "opaque-provider"
                    | "provider-booleans"
                    | "null-provider-value"
            ))
        {
            continue;
        }
        let expected = &oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["output"];
        let mut root = defaults.clone();
        merge(
            &mut root,
            &parse_root(input["yaml"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        let read = ModelSettings::read(&root);
        if let Some(error) = expected.get("error") {
            assert_eq!(read.unwrap_err().0, error.as_str().unwrap(), "{id}");
        } else {
            let settings = read.unwrap_or_else(|error| panic!("{id}: {error}"));
            let snapshot = settings.snapshot();
            for (path, expected) in expected["value"]["selected"].as_object().unwrap() {
                if path == "modelProfileResolutions" {
                    let values: serde_json::Map<String, Value> = input["selectors"].as_array().unwrap().iter().map(|selector| {
                        let selector = selector.as_str().unwrap(); let profile = settings.model_profiles.resolve(selector);
                        (selector.to_owned(),json!({"usage":profile.usage,"systemPrompt":profile.system_prompt,"maximumInputTokens":profile.maximum_input_tokens,"summaryOutputTokens":profile.summary_output_tokens}))
                    }).collect();
                    assert_eq!(&Value::Object(values), expected, "{id}:{path}");
                } else if matches!(
                    path.split('.').next(),
                    Some(
                        "modelAliases"
                            | "modelPresets"
                            | "providerModelAliasDefaults"
                            | "providers"
                            | "contextLimit"
                    )
                ) {
                    let actual = path.split('.').fold(&snapshot, |value, part| &value[part]);
                    assert_eq!(actual, expected, "{id}:{path}");
                }
            }
        }
        count += 1;
    }
    assert_eq!(count, 19);
    // Both pinned Rust libraries publish Unicode 16.0.0 tables. The independent
    // source fixture above tests actual .NET combining/ZWJ/control behavior.
    assert_eq!(unicode_segmentation::UNICODE_VERSION, (16, 0, 0));
    assert_eq!(unicode_general_category::UNICODE_VERSION, (16, 0, 0));
}

#[test]
fn aliases_preserve_exact_empty_selector_usage_augmentation_and_context() {
    let settings = read_alias(vec![
        ("usage", s("  allowed usage  ")),
        ("model_string", s("")),
        ("augment_system_prompt", s("~")),
        ("context_limit", s("20%")),
    ]);
    let alias = &settings.model_aliases["custom"];
    assert_eq!(alias.model_string, "");
    assert_eq!(alias.usage, "  allowed usage  ");
    assert_eq!(alias.augment_system_prompt.as_deref(), Some("~"));
    assert_eq!(
        alias.context_limit,
        Some(ContextSize::parse("20%").unwrap())
    );
    for null in ["null", "Null", "NULL", "", "~"] {
        let settings = read_alias(vec![
            ("usage", s("Keep")),
            ("augment_system_prompt", quoted(null)),
        ]);
        assert_eq!(
            settings.model_aliases["custom"]
                .augment_system_prompt
                .as_deref(),
            if null == "null" { None } else { Some(null) }
        );
    }
}

#[test]
fn alias_validation_errors_are_source_shaped() {
    let cases = [
        (vec![], "model_aliases.custom.usage must not be empty"),
        (
            vec![("usage", Node::sequence(vec![]))],
            "model_aliases.custom.usage must be a string",
        ),
        (
            vec![("usage", s("Keep")), ("unknown", s("x"))],
            "model_aliases.custom contains an unsupported key",
        ),
        (
            vec![("usage", s("Keep")), ("augment_system_prompt", map(vec![]))],
            "model_aliases.custom.augment_system_prompt must be a string or null",
        ),
        (
            vec![("usage", s("Keep")), ("model_string", s("provider//model"))],
            "model_aliases.custom.model_string must not contain empty path segments",
        ),
    ];
    for (fields, expected) in cases {
        assert_eq!(
            ModelSettings::read(&root("model_aliases", alias(fields)))
                .unwrap_err()
                .0,
            expected
        );
    }
    assert_eq!(
        validate_alias_name("").unwrap_err().0,
        "model_aliases key must not be empty"
    );
    assert_eq!(
        validate_alias_name(" a").unwrap_err().0,
        "model alias name \" a\" must not have surrounding whitespace"
    );
    assert_eq!(
        validate_alias_name("a/b").unwrap_err().0,
        "model alias name \"a/b\" must not contain '/'"
    );
    assert!(validate_alias_name("a b").is_ok());
}

#[test]
fn selectors_keep_distinct_alias_requested_and_provider_rules() {
    assert!(validate_model_selector("model", "provider/a b", false).is_ok());
    assert!(validate_model_selector("model", "provider/a\u{1}b", false).is_ok());
    assert!(validate_model_selector("model", "", true).is_ok());
    assert_eq!(
        validate_model_selector("model", "p/a\nb", false)
            .unwrap_err()
            .0,
        "model must not contain control whitespace"
    );
    assert_eq!(
        validate_model_selector("model", "alias", false)
            .unwrap_err()
            .0,
        "model must be provider/model"
    );
    assert!(validate_requested_selector("model", "alias").is_ok());
    assert!(validate_requested_selector("model", "provider/a b").is_err());
    assert!(validate_requested_selector("model", "provider/a\u{1}b").is_err());
    for name in ["", "a/b", "a b", "a\u{1}b", " a"] {
        assert!(validate_preset_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn alias_icons_use_one_visible_grapheme_and_supported_color() {
    for glyph in ["🦜", "e\u{301}", "👩‍👩‍👧‍👦", "🇲🇾", "◆"] {
        let settings = read_alias(vec![
            ("usage", s("Keep")),
            (
                "icon",
                map(vec![("glyph", s(glyph)), ("color", s("green"))]),
            ),
        ]);
        assert_eq!(
            settings.model_aliases["custom"]
                .icon
                .as_ref()
                .unwrap()
                .glyph,
            glyph
        );
    }
    for glyph in ["ab", "\u{301}", "\u{200d}", "\n", " ", "\u{fe0f}"] {
        let error = ModelSettings::read(&root(
            "model_aliases",
            alias(vec![
                ("usage", s("Keep")),
                (
                    "icon",
                    map(vec![("glyph", s(glyph)), ("color", s("green"))]),
                ),
            ]),
        ))
        .unwrap_err();
        assert_eq!(
            error.0, "model_aliases.custom.icon.glyph must be one visible grapheme",
            "{glyph:?}"
        );
    }
    let empty = read_alias(vec![
        ("usage", s("Keep")),
        ("icon", map(vec![("glyph", s("")), ("color", s("red"))])),
    ]);
    assert!(empty.model_aliases["custom"].icon.is_none());
    for icon in [s(""), s("null"), quoted("NULL"), Node::null()] {
        assert!(
            read_alias(vec![("usage", s("Keep")), ("icon", icon)]).model_aliases["custom"]
                .icon
                .is_none()
        );
    }
    let error = ModelSettings::read(&root(
        "model_aliases",
        alias(vec![
            ("usage", s("Keep")),
            ("icon", map(vec![("glyph", s("")), ("color", s("orange"))])),
        ]),
    ))
    .unwrap_err();
    assert_eq!(
        error.0,
        "model_aliases.custom.icon.color must be a supported color"
    );
}

#[test]
fn profiles_resolve_per_field_and_preserve_quoted_null_empty_values() {
    let profiles = map(vec![
        (
            "provider",
            map(vec![
                ("usage", s("Provider")),
                ("system_prompt", s("Prompt")),
                ("maximum_input_tokens", s("100")),
            ]),
        ),
        (
            "provider/model",
            map(vec![
                ("usage", s("null")),
                ("system_prompt", quoted("")),
                ("summary_output_tokens", s("5")),
            ]),
        ),
        (
            "provider/model/high",
            map(vec![
                ("usage", quoted("null")),
                ("maximum_input_tokens", s("~")),
            ]),
        ),
    ]);
    let settings = ModelSettings::read(&root("model_profiles", profiles)).unwrap();
    let profile = settings.model_profiles.resolve("provider/model/high");
    assert_eq!(profile.usage.as_deref(), Some("null"));
    assert_eq!(profile.system_prompt.as_deref(), Some(""));
    assert_eq!(profile.maximum_input_tokens, Some(100));
    assert_eq!(profile.summary_output_tokens, Some(5));
    assert!(
        settings
            .model_profiles
            .resolve("Provider/model")
            .usage
            .is_none()
    );
    for value in ["0", "+1", " 1 ", "2147483648", "١"] {
        let error = ModelSettings::read(&root(
            "model_profiles",
            map(vec![("p", map(vec![("maximum_input_tokens", s(value))]))]),
        ))
        .unwrap_err();
        assert_eq!(
            error.0,
            "model_profiles.p.maximum_input_tokens must be a positive integer"
        );
    }
}

#[test]
fn profiles_reject_deprecated_key_and_bad_prefixes() {
    let mut configured = root("model_profiles", map(vec![]));
    configured.set("model_augment_system_prompts", Node::null());
    assert_eq!(
        ModelSettings::read(&configured).unwrap_err().0,
        "model_augment_system_prompts is no longer supported; move each entry to model_profiles.<selector>.system_prompt"
    );
    for selector in ["", " p", "p/", "p//m", "p/\tm"] {
        assert!(
            ModelSettings::read(&root("model_profiles", map(vec![(selector, map(vec![]))])))
                .is_err()
        );
    }
}

#[test]
fn presets_and_context_limits_preserve_absence_null_and_explicit_clear() {
    let mut configured = root(
        "model_presets",
        map(vec![
            (
                "legacy",
                map(vec![
                    ("model", s("high_llm")),
                    ("model_aliases", map(vec![])),
                ]),
            ),
            (
                "empty",
                map(vec![
                    ("model", s("p/m")),
                    ("model_aliases", map(vec![("high_llm", s(""))])),
                    ("context_limits", map(vec![])),
                ]),
            ),
        ]),
    );
    configured.set("context_limit", s("100k"));
    configured.set(
        "model_aliases",
        alias(vec![("usage", s("Keep")), ("context_limit", s("20%"))]),
    );
    let mut settings = ModelSettings::read(&configured).unwrap();
    assert!(settings.model_presets["legacy"].context_limits.is_none());
    assert!(settings.model_presets["empty"].context_limits.is_some());
    let limits = settings.capture_context_limits();
    assert_eq!(limits.default.as_ref().unwrap().to_string(), "100000");
    assert_eq!(limits.model_aliases["custom"].to_string(), "20%");
    let empty = ModelPresetContextLimits {
        default: None,
        model_aliases: BTreeMap::new(),
    };
    apply_context_limits(&mut configured, &mut settings.model_aliases, &empty).unwrap();
    assert_eq!(
        configured.get("context_limit").unwrap().as_str(),
        Some("null")
    );
    assert!(settings.model_aliases["custom"].context_limit.is_none());
    assert_eq!(settings.model_aliases["custom"].usage, "Keep");
    assert_eq!(
        configured
            .get("model_aliases")
            .unwrap()
            .get("custom")
            .unwrap()
            .get("context_limit")
            .unwrap()
            .as_str(),
        Some("null")
    );
    let fields = map(vec![("context_limits", write_context_limits(&limits))]);
    assert_eq!(
        read_context_limits(&fields, "preset.context_limits").unwrap(),
        Some(limits)
    );
}

#[test]
fn context_limits_reject_invalid_values_but_exact_source_null_spellings_clear() {
    for node in [Node::null(), s("null"), quoted("null"), s("~")] {
        assert_eq!(
            read_context_limit(&map(vec![("limit", node)]), "limit", "limit").unwrap(),
            None
        );
    }
    for value in ["", "Null", "NULL", "0", "1.5k", "101%", "9223372036854776k"] {
        assert!(
            read_context_limit(&map(vec![("limit", s(value))]), "limit", "limit").is_err(),
            "{value}"
        );
    }
    assert_eq!(
        read_context_limit(&map(vec![("limit", map(vec![]))]), "limit", "limit")
            .unwrap_err()
            .0,
        "limit must be a context size or null"
    );
    let mut configured = root("model_aliases", alias(vec![("usage", s("Keep"))]));
    let mut aliases = ModelSettings::read(&configured).unwrap().model_aliases;
    let limits = ModelPresetContextLimits {
        default: None,
        model_aliases: [("missing".into(), ContextSize::parse("1k").unwrap())].into(),
    };
    assert_eq!(
        apply_context_limits(&mut configured, &mut aliases, &limits)
            .unwrap_err()
            .0,
        "model alias \"missing\" is not defined"
    );
}

#[test]
fn provider_alias_defaults_require_four_targets_for_the_same_provider() {
    let targets = |provider: &str| {
        map(vec![
            ("low_llm", s(&format!("{provider}/low"))),
            ("medium_llm", s(&format!("{provider}/medium"))),
            ("high_llm", s(&format!("{provider}/high"))),
            ("xhigh_llm", s(&format!("{provider}/xhigh"))),
        ])
    };
    let configured = map(vec![(
        "provider_model_alias_defaults",
        map(vec![("p", targets("p"))]),
    )]);
    let settings = ModelSettings::read(&configured).unwrap();
    assert_eq!(
        settings.provider_model_alias_defaults["p"].high_model_string,
        "p/high"
    );
    let wrong = map(vec![(
        "provider_model_alias_defaults",
        map(vec![("p", targets("q"))]),
    )]);
    assert_eq!(
        ModelSettings::read(&wrong).unwrap_err().0,
        "provider_model_alias_defaults.p.low_llm must select provider p"
    );
    let missing = map(vec![(
        "provider_model_alias_defaults",
        map(vec![("p", map(vec![]))]),
    )]);
    assert_eq!(
        ModelSettings::read(&missing).unwrap_err().0,
        "provider_model_alias_defaults.p must define exactly low_llm, medium_llm, high_llm, and xhigh_llm"
    );
}

#[test]
fn providers_keep_defaults_string_maps_and_tolerant_model_metadata() {
    let models = map(vec![
        (
            "m",
            map(vec![
                ("name", s("Visible")),
                ("context", s(" +42 ")),
                ("max_output_tokens", s("invalid")),
                ("max_input_tokens", s("0")),
                ("input_price", s("1.5e-3")),
                ("cached_input_price", s("-0")),
                ("tools", s("true")),
                ("reasoning", s("TRUE")),
                (
                    "output",
                    Node::sequence(vec![s("text"), map(vec![]), Node::null()]),
                ),
                (
                    "variants",
                    map(vec![
                        ("high", map(vec![("reasoning_effort", s("high"))])),
                        ("ignored", s("bad")),
                    ]),
                ),
            ]),
        ),
        ("ignored", s("bad")),
    ]);
    let headers = map(vec![("Header", s("value")), ("Ignored", map(vec![]))]);
    let settings = ModelSettings::read(&root(
        "providers",
        map(vec![
            ("p", map(vec![("models", models), ("headers", headers)])),
            ("ignored", s("bad")),
        ]),
    ))
    .unwrap();
    assert_eq!(settings.providers.len(), 1);
    let p = &settings.providers["p"];
    assert!(p.disable_websocket && p.image_urls);
    assert!(!p.allow_insecure_remote);
    assert_eq!(
        (
            p.stream_idle_timeout_ms,
            p.header_timeout_ms,
            p.header_timeout_max_retries
        ),
        (300000, 60000, 5)
    );
    assert_eq!(p.headers.len(), 1);
    let model = &p.models["m"];
    assert_eq!(model.context, 42);
    assert_eq!(model.max_output_tokens, 0);
    assert_eq!(model.input_price, 0.0015);
    assert!(model.tools && !model.reasoning);
    assert_eq!(model.output, ["text", ""]);
    assert_eq!(model.variants["high"], "high");
    assert!(model.fields.contains(ModelConfigFields::MAX_INPUT_TOKENS));
    assert!(!model.fields.contains(ModelConfigFields::OUTPUT_PRICE));
}

#[test]
fn provider_boolean_timeout_and_numeric_errors_keep_exact_paths() {
    for (field, value, expected) in [
        (
            "disable_websocket",
            "True",
            "providers.p.disable_websocket must be true or false",
        ),
        (
            "image_urls",
            "null",
            "providers.p.image_urls must be true or false",
        ),
        (
            "header_timeout_ms",
            "+1",
            "providers.p.header_timeout_ms must be a non-negative integer",
        ),
        (
            "stream_idle_timeout_ms",
            "2147483648",
            "providers.p.stream_idle_timeout_ms must be a non-negative integer",
        ),
    ] {
        assert_eq!(
            ModelSettings::read(&root(
                "providers",
                map(vec![("p", map(vec![(field, s(value))]))])
            ))
            .unwrap_err()
            .0,
            expected
        );
    }
    for value in ["NaN", "Infinity", "-1", "1e9999", "not-number"] {
        let model = map(vec![("m", map(vec![("input_price", s(value))]))]);
        assert_eq!(
            ModelSettings::read(&root(
                "providers",
                map(vec![("p", map(vec![("models", model)]))])
            ))
            .unwrap_err()
            .0,
            "models.m.input_price must be a finite non-negative number"
        );
    }
    for value in ["-0", "+1", " 1 ", "2147483648"] {
        let model = map(vec![("m", map(vec![("max_input_tokens", s(value))]))]);
        assert_eq!(
            ModelSettings::read(&root(
                "providers",
                map(vec![("p", map(vec![("models", model)]))])
            ))
            .unwrap_err()
            .0,
            "models.m.max_input_tokens must be a non-negative integer"
        );
    }
}

#[test]
fn opaque_provider_json_and_snapshot_keep_source_field_names() {
    let mut preferences = quoted("true");
    preferences.style = Style::DoubleQuoted;
    let settings = ModelSettings::read(&root(
        "providers",
        map(vec![(
            "p",
            map(vec![("provider_preferences", preferences)]),
        )]),
    ))
    .unwrap();
    assert_eq!(settings.providers["p"].provider_preferences, "true");
    let nested = map(vec![("enabled", quoted("true")), ("value", s("001"))]);
    let settings = ModelSettings::read(&root(
        "providers",
        map(vec![("p", map(vec![("provider_preferences", nested)]))]),
    ))
    .unwrap();
    assert_eq!(
        settings.providers["p"].provider_preferences,
        "{\"enabled\":true,\"value\":1}"
    );
    let snapshot = settings.snapshot();
    assert!(snapshot.get("modelAliases").is_some());
    assert_eq!(snapshot["providers"]["p"]["disableWebSocket"], true);
    assert_eq!(snapshot["providers"]["p"]["modelsDevId"], "");
    assert_eq!(snapshot["contextLimit"], serde_json::Value::Null);
}
