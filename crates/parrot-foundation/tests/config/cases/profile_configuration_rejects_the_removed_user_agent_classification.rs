use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert!(
        context
            .configured("profiles:\n  build:\n    is_user_agent: true\n")
            .is_err()
    );
}
