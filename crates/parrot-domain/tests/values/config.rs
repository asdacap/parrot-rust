use std::collections::BTreeMap;

use parrot_domain::values::config::{
    ModelProfileConfig, ModelProfiles, PathNormalizationError, PathPlatform, PlatformPath,
    SkillConfigEntry, SkillConfiguration, SkillConfigurationError,
};

fn profile(
    usage: Option<&str>,
    system_prompt: Option<&str>,
    maximum_input_tokens: Option<i32>,
    summary_output_tokens: Option<i32>,
) -> ModelProfileConfig {
    ModelProfileConfig::new(
        usage.map(str::to_owned),
        system_prompt.map(str::to_owned),
        maximum_input_tokens,
        summary_output_tokens,
    )
}

#[test]
fn model_profiles_resolve_each_field_from_the_most_specific_prefix() {
    let profiles = BTreeMap::from([
        (
            "provider".to_owned(),
            profile(
                Some("provider usage"),
                Some("provider prompt"),
                Some(80_000),
                Some(14_000),
            ),
        ),
        (
            "provider/vendor/model".to_owned(),
            profile(Some("model usage"), None, None, Some(15_000)),
        ),
        (
            "provider/vendor/model/high".to_owned(),
            profile(Some(""), Some(""), Some(70_000), None),
        ),
    ]);
    let resolved = ModelProfiles::new(&profiles).resolve("provider/vendor/model/high");
    assert_eq!(
        resolved,
        profile(Some(""), Some(""), Some(70_000), Some(15_000))
    );

    let lower_variant = ModelProfiles::new(&profiles).resolve("provider/vendor/model/low");
    assert_eq!(
        lower_variant,
        profile(
            Some("model usage"),
            Some("provider prompt"),
            Some(80_000),
            Some(15_000)
        )
    );
}

#[test]
fn model_profiles_use_ordinal_keys_copy_the_input_and_resolve_empty_selectors_to_none() {
    let mut input = BTreeMap::from([
        (
            "provider".to_owned(),
            profile(Some("original"), Some("prompt"), None, None),
        ),
        (
            "provider/model-extended".to_owned(),
            profile(Some("extended"), None, None, None),
        ),
    ]);
    let profiles = ModelProfiles::new(&input);
    input.insert(
        "provider".to_owned(),
        profile(Some("changed"), None, None, None),
    );

    assert_eq!(
        profiles.resolve("provider/model"),
        profile(Some("original"), Some("prompt"), None, None)
    );
    assert_eq!(
        profiles.resolve("Provider/model"),
        ModelProfileConfig::default()
    );
    assert_eq!(
        profiles.resolve("provider/model/extended"),
        profile(Some("original"), Some("prompt"), None, None)
    );
    assert_eq!(profiles.resolve(""), ModelProfileConfig::default());
}

#[test]
fn skill_configuration_defaults_enabled_and_global_disable_short_circuits_after_blank_check() {
    let configuration = SkillConfiguration::default();
    assert!(configuration.enabled);
    assert!(configuration.entries.is_empty());
    assert!(
        configuration
            .is_enabled("/does/not/exist/SKILL.md", "/", PathPlatform::Unix)
            .unwrap()
    );

    let disabled = SkillConfiguration::new(false, Vec::new());
    assert!(
        !disabled
            .is_enabled("relative/path", "not-an-absolute-base", PathPlatform::Unix)
            .unwrap()
    );
    assert_eq!(
        disabled.is_enabled(" \t\n", "/", PathPlatform::Unix),
        Err(SkillConfigurationError::BlankPath)
    );
    for blank in ["", " ", "\t\r\n"] {
        assert_eq!(
            configuration.is_enabled(blank, "/", PathPlatform::Unix),
            Err(SkillConfigurationError::BlankPath)
        );
    }
}

#[test]
fn skill_configuration_uses_last_normalized_duplicate_and_does_not_require_files() {
    let path = "/nonexistent/work/skill/SKILL.md";
    let entries = vec![
        SkillConfigEntry::new("/nonexistent/work/skills/../skill/SKILL.md", false),
        SkillConfigEntry::new(path, true),
        SkillConfigEntry::new("/nonexistent/work/skill/./SKILL.md", false),
    ];
    let configuration = SkillConfiguration::new(true, entries);

    assert!(
        !configuration
            .is_enabled(path, "/", PathPlatform::Unix)
            .unwrap()
    );
    assert!(
        configuration
            .is_enabled("/nonexistent/work/skill/OTHER.md", "/", PathPlatform::Unix)
            .unwrap()
    );
    assert!(
        configuration
            .is_enabled("/nonexistent/work/skill/skill.md", "/", PathPlatform::Unix)
            .unwrap()
    );
}

#[test]
fn platform_path_normalizes_lexically_without_filesystem_access() {
    assert_eq!(
        PlatformPath::normalize(
            "./skills/../skill/SKILL.md",
            "/missing/work",
            PathPlatform::Unix
        )
        .unwrap(),
        "/missing/work/skill/SKILL.md"
    );
    assert_eq!(
        PlatformPath::normalize("/../../tmp/./skill", "/", PathPlatform::Unix).unwrap(),
        "/tmp/skill"
    );
    assert_eq!(
        PlatformPath::normalize("/var/tmp/SKILL.md", "/", PathPlatform::Unix).unwrap(),
        "/var/tmp/SKILL.md"
    );
    assert_eq!(
        PlatformPath::normalize("./skills/", "/missing/work", PathPlatform::Unix).unwrap(),
        "/missing/work/skills/"
    );
    assert_eq!(
        PlatformPath::normalize("/../../tmp/", "/", PathPlatform::Unix).unwrap(),
        "/tmp/"
    );
    assert_eq!(
        PlatformPath::normalize("child", "relative-base", PathPlatform::Unix),
        Err(PathNormalizationError::BaseMustBeAbsolute)
    );
    assert_eq!(
        PlatformPath::normalize("bad\0path", "/", PathPlatform::Unix),
        Err(PathNormalizationError::NulByte)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_full_path_normalization_matches_dotnet_trailing_separator_behavior() {
    for (path, expected) in [
        ("/tmp/skill/.", "/tmp/skill"),
        ("/tmp/skill/..", "/tmp"),
        ("/tmp/skill/", "/tmp/skill/"),
    ] {
        assert_eq!(
            PlatformPath::normalize(path, "/", PathPlatform::Unix).unwrap(),
            expected,
            "{path}"
        );
    }
}

#[test]
fn macos_path_alias_policy_maps_only_var_tmp_and_etc_prefixes() {
    for (source, expected) in [
        ("/var", "/private/var"),
        ("/var/skills/SKILL.md", "/private/var/skills/SKILL.md"),
        ("/tmp", "/private/tmp"),
        ("/tmp/skill/SKILL.md", "/private/tmp/skill/SKILL.md"),
        ("/tmp/skill/", "/private/tmp/skill/"),
        ("/etc", "/private/etc"),
        ("/etc/skills/SKILL.md", "/private/etc/skills/SKILL.md"),
        ("/various/skill", "/various/skill"),
        ("/private/tmp/skill", "/private/tmp/skill"),
    ] {
        assert_eq!(
            PlatformPath::normalize(source, "/", PathPlatform::MacOs).unwrap(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn skill_matching_uses_macos_normalization_and_ordinal_comparison() {
    let configuration = SkillConfiguration::new(
        true,
        vec![SkillConfigEntry::new("/tmp/skill/SKILL.md", false)],
    );
    assert!(
        !configuration
            .is_enabled("/private/tmp/skill/SKILL.md", "/", PathPlatform::MacOs)
            .unwrap()
    );
    assert!(
        configuration
            .is_enabled("/private/tmp/skill/skill.md", "/", PathPlatform::MacOs)
            .unwrap()
    );
}
