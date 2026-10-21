use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert!(
        context
            .configured("profiles:\n  query:\n    status: retired\n")
            .unwrap()
            .runtime
            .profiles["query"]
            .prompt
            .contains("query mode")
    );
}
