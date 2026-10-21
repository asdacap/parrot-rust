use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context
        .configured("cli_utilities:\n  expected: !replace [custom]\n  optional: !replace []\n")
        .unwrap();
    assert_eq!(configuration.runtime.cli_utilities.expected, vec!["custom"]);
    assert!(configuration.runtime.cli_utilities.optional.is_empty());
}
