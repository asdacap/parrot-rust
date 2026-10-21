use crate::support::Context;
#[test]
fn source_assertions() {
    let missing = Context::new();
    let defaults = missing.load().unwrap().runtime.cli_utilities;
    let context = Context::new();
    let candidates = context.configured("cli_utilities:\n  expected:\n    - custom\n    - shared\n  optional:\n    - other\n    - shared\n").unwrap().runtime.cli_utilities;
    let mut expected = defaults.expected.clone();
    expected.extend(["custom".to_owned(), "shared".to_owned()]);
    assert_eq!(candidates.expected, expected);
    let mut optional = defaults.optional.clone();
    optional.push("other".to_owned());
    assert_eq!(candidates.optional, optional);
    let expected_only = context
        .configured("cli_utilities:\n  expected: [docker]\n")
        .unwrap()
        .runtime
        .cli_utilities;
    let mut expected = defaults.expected.clone();
    expected.push("docker".to_owned());
    assert_eq!(expected_only.expected, expected);
    assert_eq!(expected_only.optional.len(), defaults.optional.len() - 1);
    assert!(!expected_only.optional.iter().any(|value| value == "docker"));
}
