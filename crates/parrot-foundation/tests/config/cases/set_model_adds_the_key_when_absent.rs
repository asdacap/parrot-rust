use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    context
        .configured("theme: dark\n")
        .unwrap()
        .set_model("added")
        .unwrap();
    assert_eq!(context.load().unwrap().model, "added");
    assert!(context.saved().contains("theme: dark"));
}
