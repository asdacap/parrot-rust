use crate::support::Context;
#[test]
fn source_assertions() {
    for id in ["explorer", "review", "worker", "thinker", "custom"] {
        let context = Context::new();
        assert!(
            context
                .configured(&format!("default_profile: {id}\n"))
                .is_err()
        );
    }
}
