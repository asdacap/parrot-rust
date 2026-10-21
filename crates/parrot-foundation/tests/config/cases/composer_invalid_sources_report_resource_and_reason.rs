use parrot_foundation::config::yaml;
#[test]
fn source_assertions() {
    for (content, reason) in [
        ("model: [", "invalid predefined configuration YAML"),
        (
            "model: one\n---\nmodel: two",
            "exactly one mapping document",
        ),
        ("- item", "exactly one mapping document"),
        ("scalar", "exactly one mapping document"),
        ("", "exactly one mapping document"),
        ("prompt_templates: []", "prompt_templates must be a mapping"),
        ("system_prompts: []", "system_prompts must be a mapping"),
        ("tools: []", "tools must be a mapping"),
        (
            "tools:\n  ? [one, two]\n  : value",
            "mapping keys at tools must be strings",
        ),
    ] {
        let error = yaml::compose(&[("invalid.yaml", Some(content))])
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid.yaml"));
        assert!(error.contains(reason));
    }
}
