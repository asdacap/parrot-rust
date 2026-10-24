use parrot_foundation::config::yaml;
#[test]
fn source_assertions() {
    for (first, second, path) in [
        ("model: first", "model: first", "model"),
        ("model: first", "model: second", "model"),
        (
            "system_prompts:\n  shared: first",
            "system_prompts:\n  shared: second",
            "system_prompts.shared",
        ),
        (
            "tools:\n  shared:\n    description: first",
            "tools:\n  shared:\n    description: second",
            "tools.shared",
        ),
        (
            "prompt_templates:\n  shared:\n    template: first",
            "prompt_templates:\n  shared:\n    template: first",
            "prompt_templates.shared",
        ),
    ] {
        assert_conflict(first, second, path);
    }
}

#[test]
#[ignore = "abdaf0f intentionally merges disjoint fields within the same template id"]
fn source_rejects_disjoint_template_fields() {
    assert_conflict(
        "prompt_templates:\n  shared:\n    template: first",
        "prompt_templates:\n  shared:\n    allowed_arguments: []",
        "prompt_templates.shared",
    );
}

fn assert_conflict(first: &str, second: &str, path: &str) {
    let error =
        yaml::compose(&[("first.yaml", Some(first)), ("second.yaml", Some(second))]).unwrap_err();
    for expected in [
        "duplicate predefined configuration",
        "first.yaml",
        "second.yaml",
        path,
    ] {
        assert!(error.0.contains(expected), "{} missing {expected}", error.0);
    }
}
