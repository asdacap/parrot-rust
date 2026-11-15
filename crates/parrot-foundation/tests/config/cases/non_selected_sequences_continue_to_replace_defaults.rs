use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture.configured("profiles:\n  thinker:\n    allowed_tools: [read]\nproviders:\n  chatgpt:\n    model_defaults:\n      gpt-5.6-sol:\n        output: [custom]\n").unwrap();
    assert_eq!(
        configuration.runtime.profiles["thinker"]
            .allowed_tools
            .as_deref(),
        Some(["read".to_owned()].as_slice())
    );
    assert_eq!(
        configuration.models.providers["chatgpt"].model_defaults["gpt-5.6-sol"].output,
        ["custom"]
    );
}
