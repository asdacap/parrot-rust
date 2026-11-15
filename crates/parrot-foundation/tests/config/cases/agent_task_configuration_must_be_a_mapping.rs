use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for value in ["null", "5", "[]"] {
        let fixture = ConfigFixture::new();
        assert_eq!(
            fixture
                .configured(&format!("agent_tasks: {value}\n"))
                .unwrap_err()
                .to_string(),
            "agent_tasks must be a mapping"
        );
    }
}
