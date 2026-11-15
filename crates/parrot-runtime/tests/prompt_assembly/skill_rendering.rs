use super::*;

fn skill(index: usize) -> SkillMetadata {
    SkillMetadata {
        name: format!("skill-{index}"),
        description: "description".into(),
        path: format!("/skills/{index}/SKILL.md"),
        discovery_path: format!("/logical/{index}/SKILL.md"),
        scope: SkillScope::Repo,
        display_name: None,
        short_description: None,
        enabled: true,
        prompt_visible: true,
    }
}
#[test]
fn catalog_preserves_input_precedence_and_excludes_disabled_or_explicit_only_skills() {
    let mut skills = vec![skill(3), skill(1), skill(2), skill(4)];
    skills[2].enabled = false;
    skills[3].prompt_visible = false;
    let text = render_skill_catalog(&skills, &templates("")).unwrap();
    assert!(text.find("$skill-3").unwrap() < text.find("$skill-1").unwrap());
    assert!(!text.contains("$skill-2"));
    assert!(!text.contains("$skill-4"));
    assert!(
        render_skill_catalog(&[], &templates(""))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn catalog_utf8_budget_includes_wrapper_and_omission_count() {
    let skills: Vec<_> = (0..1000)
        .map(|index| {
            let mut value = skill(index);
            value.description = "界".repeat(100);
            value
        })
        .collect();
    let text = render_skill_catalog(&skills, &templates("")).unwrap();
    assert!(text.len() <= MAXIMUM_CATALOG_BYTES);
    assert!(
        text.contains(
            "additional skill(s) omitted because the catalogue reached its 64 KiB limit."
        )
    );
    assert!(std::str::from_utf8(text.as_bytes()).is_ok());
}
#[test]
fn catalog_rejects_wrapper_that_consumes_entire_source_limit() {
    let content = format!(
        "prompt_templates:\n  context.skills:\n    template: '{}{{skills}}'\n",
        "x".repeat(MAXIMUM_CATALOG_BYTES)
    );
    assert!(render_skill_catalog(&[skill(1)], &templates(&content)).is_err());
}

#[test]
fn valid_short_catalog_template_without_placeholder_preserves_source_signed_budget() {
    for literal in ["x", "none"] {
        let content = format!("prompt_templates:\n  context.skills:\n    template: '{literal}'\n");
        assert_eq!(
            render_skill_catalog(&[skill(1)], &templates(&content)).unwrap(),
            literal
        );
    }
}
