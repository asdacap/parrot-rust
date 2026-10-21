use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut config = context.configured("model: old\ntheme: dark\n").unwrap();
    config.set_model("new").unwrap();
    let rewritten = context.saved();
    assert!(rewritten.contains("model: new"));
    assert!(rewritten.contains("theme: dark"));
    assert!(!rewritten.contains("old"));
}
