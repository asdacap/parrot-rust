use crate::support::{ConfigFixture, transact};
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut configuration = fixture.load().unwrap();
    assert!(
        transact(&mut configuration, |transaction| {
            transaction.set_model_alias("undefined", "openai/gpt-5.6")
        })
        .is_err()
    );
}
