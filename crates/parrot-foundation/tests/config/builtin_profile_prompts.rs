use crate::fixtures;
use parrot_foundation::config::{ConfigErrorKind, runtime::PROFILE_IDS, yaml};
use parrot_foundation::template_catalog::PromptTemplateCatalog;

#[test]
fn defaults_resolve_to_profile_prompt_text() {
    let (configuration, _) =
        PromptTemplateCatalog::from_content("", &fixtures::environment()).unwrap();
    assert_eq!(configuration.runtime.profiles.len(), PROFILE_IDS.len());
    for profile in configuration.runtime.profiles.values() {
        assert!(!profile.prompt.trim().is_empty());
        assert!(!profile.prompt.starts_with("@builtin:"));
    }
    assert_eq!(
        configuration.runtime.profiles["general"].prompt,
        include_str!("../../../../assets/config/profile-prompts/general.md")
    );
    assert_eq!(
        configuration.runtime.profiles["agent-task-payload"].prompt,
        include_str!("../../../../assets/config/profile-prompts/agent-task-payload.md")
    );
}

#[test]
fn inline_profile_overrides_preserve_inherited_settings_and_other_prompts() {
    let environment = fixtures::environment();
    let (mut inherited, _) = PromptTemplateCatalog::from_content("", &environment).unwrap();
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "profiles:\n  plan:\n    prompt: Custom plan guidance\n    max_turns: 7\n",
        &environment,
    )
    .unwrap();
    let expected = inherited.runtime.profiles.get_mut("plan").unwrap();
    expected.prompt = "Custom plan guidance".into();
    expected.max_turns = 7;
    assert_eq!(configuration.runtime.profiles, inherited.runtime.profiles);
    assert_eq!(configuration.system_prompts, inherited.system_prompts);
}

#[test]
fn known_references_can_be_reused_across_prompt_fields() {
    let environment = fixtures::environment();
    let (inherited, _) = PromptTemplateCatalog::from_content("", &environment).unwrap();
    for id in PROFILE_IDS {
        let (configuration, _) = PromptTemplateCatalog::from_content(
            &format!("profiles:\n  build:\n    prompt: '@builtin:profile-prompts/{id}.md'\n"),
            &environment,
        )
        .unwrap();
        assert_eq!(
            configuration.runtime.profiles["build"].prompt, inherited.runtime.profiles[id].prompt,
            "{id}"
        );
    }
    let (configuration, _) = PromptTemplateCatalog::from_content(
        "profiles:\n  build:\n    prompt: '@builtin:system-prompts/base.md'\nsystem_prompts:\n  custom:guidance: '@builtin:profile-prompts/query.md'\n",
        &environment,
    )
    .unwrap();
    assert_eq!(
        configuration.runtime.profiles["build"].prompt,
        inherited.system_prompts["runtime:system-context:01-base"]
    );
    assert_eq!(
        configuration.system_prompts["custom:guidance"],
        inherited.runtime.profiles["query"].prompt
    );
}

#[test]
fn invalid_profile_references_report_the_profile_prompt_field() {
    for reference in [
        "@builtin:",
        "@builtin:profile-prompts/missing.md",
        "@builtin:profile-prompts/build.md ",
        "@builtin:profile-prompts/build.md/extra",
        "@builtin:profile-prompts/../build.md",
        "@builtin:/profile-prompts/build.md",
        "@builtin:profile-prompts/Build.md",
    ] {
        let content = format!(
            "profiles:\n  build:\n    prompt: {}\n",
            serde_json::to_string(reference).unwrap()
        );
        let error = PromptTemplateCatalog::from_content(&content, &fixtures::environment())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("profiles.build.prompt"), "{message}");
        assert!(message.contains(reference), "{message}");
    }
}

#[test]
fn profile_strings_without_the_exact_initial_prefix_remain_literal() {
    for text in [
        "Explain @builtin:profile-prompts/build.md literally.",
        " @builtin:profile-prompts/build.md",
        "@Builtin:profile-prompts/build.md",
        "@builtin",
        "{name} and {{ literal }}\n@builtin:profile-prompts/build.md\n",
    ] {
        let content = format!(
            "profiles:\n  build:\n    prompt: {}\n",
            serde_json::to_string(text).unwrap()
        );
        let (configuration, _) =
            PromptTemplateCatalog::from_content(&content, &fixtures::environment()).unwrap();
        assert_eq!(configuration.runtime.profiles["build"].prompt, text);
    }
}

#[test]
fn profile_prompt_values_still_require_nonempty_strings() {
    for value in ["''", "'   '", "null", "~", "[]", "{}"] {
        let content = format!("profiles:\n  build:\n    prompt: {value}\n");
        let error = PromptTemplateCatalog::from_content(&content, &fixtures::environment())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
        assert!(error.to_string().contains("profiles.build.prompt"));
    }
}

#[test]
fn nonprompt_fields_and_model_guidance_do_not_expand_references() {
    let reference = "@builtin:profile-prompts/query.md";
    let (configuration, _) = PromptTemplateCatalog::from_content(
        &format!(
            "profiles:\n  build:\n    usage: '{reference}'\nmodel_profiles:\n  custom/model:\n    system_prompt: '{reference}'\nmodel_aliases:\n  low_llm:\n    augment_system_prompt: '{reference}'\n"
        ),
        &fixtures::environment(),
    )
    .unwrap();
    assert_eq!(configuration.runtime.profiles["build"].usage, reference);
    assert_eq!(
        configuration.models.model_profile_entries["custom/model"].system_prompt,
        Some(reference.into())
    );
    assert_eq!(
        configuration.models.model_aliases["low_llm"].augment_system_prompt,
        Some(reference.into())
    );
}

#[test]
fn loading_keeps_profile_references_and_preserves_user_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.yaml");
    let predefined = directory.path().join("predefined_config.yaml");
    let content =
        b"# Preserve this formatting.\nprofiles:\n  build:\n    prompt: Custom build guidance\n";
    std::fs::write(&path, content).unwrap();
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
    assert_eq!(
        configuration.runtime.profiles["build"].prompt,
        "Custom build guidance"
    );
    let root = yaml::parse_root(&std::fs::read_to_string(&predefined).unwrap()).unwrap();
    for id in PROFILE_IDS {
        assert_eq!(
            root.get("profiles")
                .unwrap()
                .get(id)
                .unwrap()
                .get("prompt")
                .unwrap()
                .as_str(),
            Some(format!("@builtin:profile-prompts/{id}.md").as_str())
        );
        assert!(
            !configuration.runtime.profiles[id]
                .prompt
                .starts_with("@builtin:")
        );
    }
}
