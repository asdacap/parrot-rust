use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for field in [
        "is_user_selectable: yes",
        "is_user_selectable: 1",
        "is_agent_selectable: yes",
        "is_agent_selectable: 1",
    ] {
        let fixture = ConfigFixture::new();
        let _: parrot_foundation::config::ConfigError = fixture
            .configured(&format!("profiles:\n  worker:\n    {field}\n"))
            .unwrap_err();
    }
}
