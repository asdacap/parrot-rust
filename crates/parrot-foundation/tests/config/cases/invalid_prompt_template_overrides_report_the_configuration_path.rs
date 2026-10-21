use crate::support::Context;
#[test]
fn source_assertions() {
    for (override_yaml, reason) in [
        ("template: ''", "must be a non-empty string"),
        (
            "required_arguments: [path, absent]",
            "must be included in allowed_arguments",
        ),
    ] {
        let context = Context::new();
        let error = context
            .configured(&format!(
                "prompt_templates:\n  tool-result.output-blob-notice:\n    {override_yaml}\n"
            ))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("prompt_templates.tool-result.output-blob-notice"));
        assert!(error.contains(reason));
    }
}
