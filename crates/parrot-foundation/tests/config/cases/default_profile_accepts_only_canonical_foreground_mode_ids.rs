use crate::support::Context;
#[test]
fn source_assertions() {
    for id in ["coordinator", "build", "plan", "query"] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!("default_profile: {id}\n"))
                .unwrap()
                .runtime
                .default_profile,
            id
        );
    }
}
