use crate::support::{Context, transact};
#[test]
fn source_assertions() {
    let context = Context::new();
    let mut configuration = context.load().unwrap();
    assert!(
        transact(&mut configuration, |transaction| {
            transaction.set_model_alias("undefined", "openai/gpt-5.6")
        })
        .is_err()
    );
}
