use crate::support::Context;
#[test]
fn source_assertions() {
    let missing = Context::new();
    assert!(missing.load().unwrap().runtime.inline_diff);
    for (value, expected) in [("true", true), ("false", false)] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!("inline_diff: {value}\n"))
                .unwrap()
                .runtime
                .inline_diff,
            expected
        );
    }
}
