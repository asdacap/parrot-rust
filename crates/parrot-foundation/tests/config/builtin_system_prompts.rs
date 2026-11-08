use crate::fixtures;
use parrot_foundation::config::{ConfigErrorKind, templates, yaml};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

#[test]
fn defaults_resolve_to_prompt_text() {
    let (configuration, _) =
        PromptTemplateCatalog::from_content("", &fixtures::environment()).unwrap();
    assert_eq!(configuration.system_prompts.len(), 7);
    for text in configuration.system_prompts.values() {
        assert!(!text.trim().is_empty());
        assert!(!text.starts_with("@builtin:"));
    }
    assert!(
        configuration.system_prompts["runtime:system-context:01-base"]
            .starts_with("You are parrot, a coding agent.")
    );
    assert!(
        configuration.system_prompts["runtime:system-context:02a-agent-tasks"]
            .starts_with("## Agent Tasks")
    );
}

#[test]
fn inline_overrides_replace_only_the_selected_prompt() {
    let environment = fixtures::environment();
    let (inherited, _) = PromptTemplateCatalog::from_content("", &environment).unwrap();
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "system_prompts:\n  runtime:system-context:01-base: Custom base prompt.\n  custom:guidance: Additional guidance.\n",
        &environment,
    )
    .unwrap();
    assert_eq!(configuration.system_prompts.len(), 8);
    assert_eq!(
        configuration.system_prompts["runtime:system-context:01-base"],
        "Custom base prompt."
    );
    assert_eq!(
        configuration.system_prompts["custom:guidance"],
        "Additional guidance."
    );
    for (key, text) in inherited.system_prompts {
        if key != "runtime:system-context:01-base" {
            assert_eq!(configuration.system_prompts[&key], text);
        }
    }
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
        (
            "runtime:system-context:04a-markdown-reading",
            "markdown-reading.md",
        ),
        ("runtime:system-context:05-git-ethics", "git-ethics.md"),
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
