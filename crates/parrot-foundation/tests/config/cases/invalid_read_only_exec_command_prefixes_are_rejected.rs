use crate::support::Context;
#[test]
fn source_assertions() {
    let element_error = "read_only_exec_command_prefixes[19] must be a unique nonblank string without leading or trailing whitespace";
    for (yaml, message) in [
        (
            "read_only_exec_command_prefixes: null",
            "read_only_exec_command_prefixes must be a string sequence",
        ),
        ("read_only_exec_command_prefixes:\n  - ''", element_error),
        (
            "read_only_exec_command_prefixes:\n  - '  rg'",
            element_error,
        ),
        (
            "read_only_exec_command_prefixes:\n  - rg\n  - rg",
            element_error,
        ),
        ("read_only_exec_command_prefixes:\n  - [rg]", element_error),
    ] {
        let context = Context::new();
        assert_eq!(
            context.configured(&format!("{yaml}\n")).unwrap_err().0,
            message
        );
    }
}
