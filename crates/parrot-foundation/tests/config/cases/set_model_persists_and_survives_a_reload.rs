use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    context.load().unwrap().set_model("glm-5.2").unwrap();
    assert_eq!(context.load().unwrap().model, "glm-5.2");
}
