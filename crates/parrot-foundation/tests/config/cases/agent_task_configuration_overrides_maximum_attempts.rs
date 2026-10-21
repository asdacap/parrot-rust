use crate::support::Context;
#[test]
fn source_assertions() {
    for expected in [2, i32::MAX] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!("agent_tasks:\n  maximum_attempts: {expected}\n"))
                .unwrap()
                .runtime
                .agent_tasks
                .maximum_attempts,
            expected
        );
    }
}
