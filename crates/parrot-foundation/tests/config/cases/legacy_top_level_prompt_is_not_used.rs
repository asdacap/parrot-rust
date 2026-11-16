use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture.configured("prompt: Legacy prompt.\n").unwrap();
    assert!(
        !configuration.system_prompts["runtime:system-context:01-base"].contains("Legacy prompt.")
    );
}
