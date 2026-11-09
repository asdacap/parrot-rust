use parrot_runtime::prompt_context::{
    SkillDisplayMetadata, SkillScope, normalize_skill_path, parse_skill_display_metadata,
    parse_skill_frontmatter,
};

fn skill(path: &str, name: &str) -> parrot_runtime::prompt_context::SkillMetadata {
    parse_skill_frontmatter(
        path,
        &format!("---\nname: {name}\ndescription: description\n---\nbody"),
        SkillScope::User,
        true,
    )
    .unwrap()
}

#[test]
fn frontmatter_sanitizes_defaults_and_does_not_parse_instruction_body() {
    let content = "---\r\ndescription: >-\r\n  A useful\r\n  skill\r\nmetadata:\r\n  short-description: A short description\r\n---\r\n# Instructions\n[invalid yaml:";
    let parsed = parse_skill_frontmatter(
        "/tmp/fallback-name/SKILL.md",
        content,
        SkillScope::User,
        true,
    )
    .unwrap();
    assert_eq!(parsed.name, "fallback-name");
    assert_eq!(parsed.description, "A useful skill");
    assert_eq!(
        parsed.short_description.as_deref(),
        Some("A short description")
    );
    assert_eq!(parsed.path, "/tmp/fallback-name/SKILL.md");
    assert_eq!(parsed.discovery_path, parsed.path);
    assert!(parsed.enabled && parsed.prompt_visible);
    assert_eq!(
        skill("/tmp/fallback-name/SKILL.md", "'   '").name,
        "fallback-name"
    );
}

#[test]
fn frontmatter_enforces_original_required_bounds_and_utf16_units() {
    for (description, length, valid) in [
        ("valid", 65, false),
        ("''", 4, false),
        ("valid", 64, true),
        ("a", 4, true),
    ] {
        let content = format!(
            "---\nname: {}\ndescription: {description}\n---\nbody",
            "n".repeat(length)
        );
        assert_eq!(
            parse_skill_frontmatter("/tmp/skill/SKILL.md", &content, SkillScope::Repo, true)
                .is_ok(),
            valid
        );
    }
    for content in [
        "---\nname: valid\n---\nbody".into(),
        format!(
            "---\nname: valid\ndescription: {}\n---\nbody",
            "d".repeat(1025)
        ),
    ] {
        assert!(
            parse_skill_frontmatter("/tmp/skill/SKILL.md", &content, SkillScope::User, true)
                .is_err()
        );
    }
    let name = "😀".repeat(32);
    assert_eq!(skill("/tmp/skill/SKILL.md", &name).name, name);
    assert!(
        parse_skill_frontmatter(
            "/tmp/skill/SKILL.md",
            &format!("---\nname: {name}x\ndescription: valid\n---"),
            SkillScope::User,
            true
        )
        .is_err()
    );
}

#[test]
fn malformed_frontmatter_has_source_error_categories() {
    for (content, message) in [
        (
            "not frontmatter",
            "SKILL.md must begin with YAML frontmatter.",
        ),
        (
            "---\n---",
            "SKILL.md frontmatter is not closed or is empty.",
        ),
        (
            "---\ndescription: valid",
            "SKILL.md frontmatter is not closed or is empty.",
        ),
        (
            "---\n[broken:\n---",
            "SKILL.md frontmatter contains invalid YAML.",
        ),
        (
            "---\n- valid\n---",
            "SKILL.md frontmatter contains invalid YAML.",
        ),
        (
            "---\nname: a\nname: b\ndescription: valid\n---",
            "SKILL.md frontmatter contains invalid YAML.",
        ),
    ] {
        let failure = parse_skill_frontmatter("/skill/SKILL.md", content, SkillScope::User, true)
            .unwrap_err();
        assert_eq!(failure.message, message);
        assert_eq!(failure.to_string(), format!("/skill/SKILL.md: {message}"));
    }
}

#[test]
fn frontmatter_uses_scalar_fields_only_and_explicit_invocation_flag() {
    for (flag, fallback, expected) in [
        ("", true, true),
        ("", false, false),
        ("disable-model-invocation: true\n", true, false),
        ("disable-model-invocation: false\n", false, true),
        ("disable-model-invocation: yes\n", false, false),
        ("disable-model-invocation: ' TrUe '\n", true, false),
    ] {
        let parsed = parse_skill_frontmatter(
            "/skill/SKILL.md",
            &format!("---\ndescription: valid\n{flag}---\nbody"),
            SkillScope::User,
            fallback,
        )
        .unwrap();
        assert_eq!(parsed.prompt_visible, expected);
    }
    let parsed = parse_skill_frontmatter(
        "/fallback/SKILL.md",
        "---\nname: [ignored]\ndescription: valid\nmetadata: [ignored]\n---",
        SkillScope::System,
        false,
    )
    .unwrap();
    assert_eq!(parsed.name, "fallback");
    assert_eq!(parsed.short_description, None);
    assert_eq!(parsed.scope, SkillScope::System);
}

#[test]
fn display_metadata_fails_open_and_validates_sanitized_values() {
    let parsed = parse_skill_display_metadata(
        "dependencies:\n  tools: [ignored]\npolicy:\n  allow_implicit_invocation: false\ninterface:\n  display_name: '  Display   Name  '\n  short_description: >-\n    Short\n    description",
    );
    assert_eq!(
        parsed,
        SkillDisplayMetadata {
            display_name: Some("Display Name".into()),
            short_description: Some("Short description".into()),
            allow_implicit_invocation: Some(false)
        }
    );
    assert_eq!(
        parse_skill_display_metadata(&format!(
            "interface:\n  display_name: {}\n  short_description: usable",
            "x".repeat(65)
        )),
        SkillDisplayMetadata {
            short_description: Some("usable".into()),
            ..Default::default()
        }
    );
    assert_eq!(
        parse_skill_display_metadata("policy:\n  allow_implicit_invocation: true"),
        SkillDisplayMetadata {
            allow_implicit_invocation: Some(true),
            ..Default::default()
        }
    );
    for content in [
        "interface: [",
        "[]",
        "",
        "interface: value",
        "policy: [true]",
    ] {
        assert_eq!(
            parse_skill_display_metadata(content),
            SkillDisplayMetadata::default()
        );
    }
    assert_eq!(
        parse_skill_display_metadata(&format!("interface:\n  display_name: {}", "😀".repeat(33)))
            .display_name,
        None
    );
}

#[test]
fn yaml_scalar_forms_anchors_and_block_strings_use_existing_foundation_parser() {
    let parsed = parse_skill_frontmatter("/skill/SKILL.md", "---\nname: &name skill\ndescription: *name\nmetadata:\n  short-description: |\n    line one\n    line two\n---", SkillScope::Repo, true).unwrap();
    assert_eq!(parsed.description, "skill");
    assert_eq!(
        parsed.short_description.as_deref(),
        Some("line one line two")
    );
    let display = parse_skill_display_metadata(
        "interface: {display_name: 'Display', short_description: \"line\\nnext\"}",
    );
    assert_eq!(display.short_description.as_deref(), Some("line next"));
}

#[test]
fn lexical_path_normalization_never_consults_host_or_resolves_links() {
    assert_eq!(
        normalize_skill_path("/a//b/../../skill/SKILL.md", None).unwrap(),
        "/skill/SKILL.md"
    );
    assert_eq!(
        normalize_skill_path("../skill/SKILL.md", Some("/working")).unwrap(),
        "/skill/SKILL.md"
    );
    assert!(normalize_skill_path("relative", None).is_err());
    assert!(normalize_skill_path("a\0b", Some("/working")).is_err());
    assert!(
        parse_skill_frontmatter(
            "relative/SKILL.md",
            "---\ndescription: valid\n---",
            SkillScope::User,
            true
        )
        .is_err()
    );
}
