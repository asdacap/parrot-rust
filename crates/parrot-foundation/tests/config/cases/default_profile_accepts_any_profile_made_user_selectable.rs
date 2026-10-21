use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context
        .configured("default_profile: worker\nprofiles:\n  worker:\n    is_user_selectable: true\n")
        .unwrap();
    assert_eq!(configuration.runtime.default_profile, "worker");
}
