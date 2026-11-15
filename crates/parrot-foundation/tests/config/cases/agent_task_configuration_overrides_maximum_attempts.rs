use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for expected in [2, i32::MAX] {
        let fixture = ConfigFixture::new();
        assert_eq!(
            fixture
                .configured(&format!("agent_tasks:\n  maximum_attempts: {expected}\n"))
                .unwrap()
                .runtime
                .agent_tasks
                .maximum_attempts,
            expected
        );
    }
}
