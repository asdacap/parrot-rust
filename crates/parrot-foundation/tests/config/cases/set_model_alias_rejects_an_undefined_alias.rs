use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut configuration = context.load().unwrap();
    assert!(
        configuration
            .set_model_alias("undefined", "openai/gpt-5.6")
            .is_err()
    );
}
