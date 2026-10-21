use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config = context
        .configured("cli_utilities:\n  expected: !replace [custom]\n")
        .unwrap();
    config.set_model("glm-5.2").unwrap();
    assert!(context.saved().contains("!replace"));
    assert_eq!(
        context.load().unwrap().runtime.cli_utilities.expected,
        vec!["custom"]
    );
}
