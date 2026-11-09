use crate::fixtures;
use parrot_foundation::config::{ConfigErrorKind, templates, yaml};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

#[test]
fn defaults_resolve_to_prompt_text() {
    let (configuration, _) =
        PromptTemplateCatalog::from_content("", &fixtures::environment()).unwrap();
    for text in configuration.system_prompts.values() {
        assert!(!text.trim().is_empty());
        assert!(!text.starts_with("@builtin:"));
    }
    assert_eq!(
        configuration.system_prompts["runtime:system-context:01-base"],
        include_str!("../../../../assets/config/system-prompts/base.md")
    );
    assert_eq!(
        configuration.system_prompts["runtime:system-context:02a-agent-tasks"],
        include_str!("../../../../assets/config/system-prompts/agent-tasks.md")
    );
}

#[test]
fn inline_overrides_replace_only_the_selected_prompt() {
    let environment = fixtures::environment();
    let (mut inherited, _) = PromptTemplateCatalog::from_content("", &environment).unwrap();
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "system_prompts:\n  runtime:system-context:01-base: Custom base prompt.\n  custom:guidance: Additional guidance.\n",
        &environment,
    )
    .unwrap();
    inherited.system_prompts.insert(
        "runtime:system-context:01-base".into(),
        "Custom base prompt.".into(),
    );
    inherited
        .system_prompts
        .insert("custom:guidance".into(), "Additional guidance.".into());
    assert_eq!(configuration.system_prompts, inherited.system_prompts);
}

#[test]
fn builtin_references_can_be_reused_under_custom_provider_keys() {
    let environment = fixtures::environment();
    let (inherited, _) = PromptTemplateCatalog::from_content("", &environment).unwrap();
    for (key, filename) in [
        ("runtime:system-context:01-base", "base.md"),
        ("runtime:system-context:02-delegation", "delegation.md"),
        ("runtime:system-context:02a-agent-tasks", "agent-tasks.md"),
        (
            "runtime:system-context:03-subagent-pattern",
            "subagent-patterns.md",
        ),
        (
            "runtime:system-context:04-behavior-guideline",
            "behavior-guidelines.md",
        ),
        ("runtime:system-context:05-toolings", "toolings.md"),
    ] {
        let (configuration, _) = PromptTemplateCatalog::from_content(
            &format!("system_prompts:\n  custom:guidance: '@builtin:system-prompts/{filename}'\n"),
            &environment,
        )
        .unwrap();
        assert_eq!(
            configuration.system_prompts["custom:guidance"], inherited.system_prompts[key],
            "{filename}"
        );
    }
}

#[test]
fn invalid_builtin_references_report_the_configuration_field() {
    for reference in [
        "@builtin:",
        "@builtin:system-prompts/missing.md",
        "@builtin:system-prompts/base.md ",
        "@builtin:system-prompts/base.md/extra",
        "@builtin:system-prompts/../base.md",
        "@builtin:/system-prompts/base.md",
        "@builtin:system-prompts/Base.md",
    ] {
        let content = format!(
            "system_prompts:\n  custom:guidance: {}\n",
            serde_json::to_string(reference).unwrap()
        );
        let error = PromptTemplateCatalog::from_content(&content, &fixtures::environment())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("system_prompts.custom:guidance"));
        assert!(message.contains(reference), "{message}");
    }
}

#[test]
fn strings_without_the_exact_initial_prefix_remain_literal() {
    for text in [
        "Explain @builtin:system-prompts/base.md literally.",
        " @builtin:system-prompts/base.md",
        "@Builtin:system-prompts/base.md",
        "@builtin",
        "{name} and {{ literal }}\n@builtin:system-prompts/base.md\n",
    ] {
        let content = format!(
            "system_prompts:\n  custom:guidance: {}\n",
            serde_json::to_string(text).unwrap()
        );
        let (configuration, _) =
            PromptTemplateCatalog::from_content(&content, &fixtures::environment()).unwrap();
        assert_eq!(configuration.system_prompts["custom:guidance"], text);
    }
}

#[test]
fn provider_keys_and_values_still_require_valid_namespaced_nonempty_strings() {
    for content in [
        "system_prompts: []",
        "system_prompts:\n  custom:guidance: ''",
        "system_prompts:\n  custom:guidance: '   '",
        "system_prompts:\n  custom:guidance: null",
        "system_prompts:\n  custom:guidance: []",
        "system_prompts:\n  guidance: '@builtin:system-prompts/base.md'",
        "system_prompts:\n  ':guidance': '@builtin:system-prompts/base.md'",
        "system_prompts:\n  'custom:': '@builtin:system-prompts/base.md'",
        "system_prompts:\n  ' custom:guidance': '@builtin:system-prompts/base.md'",
    ] {
        let root = yaml::parse_root(content).unwrap();
        let error = templates::read_system_prompts(&root).unwrap_err();
        assert_eq!(error.kind(), ConfigErrorKind::InvalidData, "{content}");
    }
}

#[test]
fn loading_keeps_references_in_generated_yaml_and_preserves_user_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.yaml");
    let predefined = directory.path().join("predefined_config.yaml");
    let content = b"# Preserve this comment and formatting.\nmodel: openai/gpt-5\nsystem_prompts:\n  custom:guidance: Keep this literal.\n";
    std::fs::write(&path, content).unwrap();
    std::fs::write(&predefined, "model: stale/model\n").unwrap();
    let environment = [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ]
    .map(|(name, child)| {
        (
            name.to_owned(),
            directory.path().join(child).to_string_lossy().into_owned(),
        )
    })
    .into_iter()
    .collect();
    let (configuration, _) = PromptTemplateCatalog::load(&path, &predefined, &environment).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), content);
    let reference = std::fs::read_to_string(&predefined).unwrap();
    assert!(!reference.contains("stale/model"));
    let documents = yaml::parse_documents(&reference).unwrap();
    assert_eq!(documents.len(), 1);
    let root = &documents[0];
    for (key, reference) in [
        (
            "runtime:system-context:01-base",
            "@builtin:system-prompts/base.md",
        ),
        (
            "runtime:system-context:02a-agent-tasks",
            "@builtin:system-prompts/agent-tasks.md",
        ),
    ] {
        assert_eq!(
            root.get("system_prompts")
                .unwrap()
                .get(key)
                .unwrap()
                .as_str(),
            Some(reference)
        );
    }
    assert_eq!(
        configuration.system_prompts["runtime:system-context:01-base"],
        include_str!("../../../../assets/config/system-prompts/base.md")
    );
    assert_eq!(
        configuration.system_prompts["runtime:system-context:02a-agent-tasks"],
        include_str!("../../../../assets/config/system-prompts/agent-tasks.md")
    );
    assert!(
        configuration
            .system_prompts
            .values()
            .all(|text| !text.starts_with("@builtin:"))
    );
    assert_eq!(
        configuration.system_prompts["custom:guidance"],
        "Keep this literal."
    );
    for key in ["system_prompts", "agent_tasks", "profiles", "tools"] {
        assert!(root.get(key).is_some());
    }
}
