use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture
        .configured("profiles:\n  thinker:\n    allowed_tools: []\n")
        .unwrap();
    assert_eq!(
        configuration.runtime.profiles["thinker"]
            .allowed_tools
            .as_deref(),
        Some([].as_slice())
    );
}
