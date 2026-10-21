use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("prompt: Legacy prompt.\n").unwrap();
    assert!(
        !configuration.system_prompts["runtime:system-context:01-base"].contains("Legacy prompt.")
    );
}
