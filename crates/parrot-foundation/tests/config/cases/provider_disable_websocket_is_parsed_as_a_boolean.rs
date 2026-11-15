use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for provider in ["custom", "chatgpt"] {
        for (value, expected) in [("true", true), ("false", false)] {
            let fixture = ConfigFixture::new();
            let configuration = fixture
                .configured(&format!(
                    "providers:\n  {provider}:\n    disable_websocket: {value}\n"
                ))
                .unwrap();
            assert_eq!(
                configuration.models.providers[provider].disable_websocket,
                expected
            );
        }
    }
}
