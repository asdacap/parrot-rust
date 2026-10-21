use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    assert_eq!(
        context
            .configured("profiles:\n  unknown: {}\n")
            .unwrap_err()
            .to_string(),
        "profiles.unknown is not supported"
    );
}
