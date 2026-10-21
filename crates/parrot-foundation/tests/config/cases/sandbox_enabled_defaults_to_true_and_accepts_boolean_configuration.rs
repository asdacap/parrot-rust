use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    assert!(context.load().unwrap().runtime.sandbox_enabled);
    assert!(
        context
            .configured("sandbox:\n  enabled: true\n")
            .unwrap()
            .runtime
            .sandbox_enabled
    );
    assert!(
        !context
            .configured("sandbox:\n  enabled: false\n")
            .unwrap()
            .runtime
            .sandbox_enabled
    );
}
