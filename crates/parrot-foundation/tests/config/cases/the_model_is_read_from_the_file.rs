use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert_eq!(
        context
            .configured("# parrot config\nmodel: deepseek-v4-pro\n")
            .unwrap()
            .model,
        "deepseek-v4-pro"
    );
}
