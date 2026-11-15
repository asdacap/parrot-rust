use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let error=fixture.configured("prompt_templates:\n  custom.structured:\n    engine: unknown\n    template: 'text'\n    allowed_arguments: []\n    required_arguments: []\n").unwrap_err().to_string();
    assert!(error.starts_with("prompt_templates.custom.structured."));
}
