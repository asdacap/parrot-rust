use crate::support::Context;
#[test]
fn source_assertions() {
    for (key, value) in [
        ("maximum_attempts", "0"),
        ("maximum_attempts", "-1"),
        ("maximum_attempts", "1.5"),
        ("maximum_attempts", "true"),
        ("maximum_attempts", "null"),
        ("maximum_attempts", "words"),
        ("maximum_attempts", "2147483648"),
        ("maximum_response_repairs", "0"),
        ("maximum_response_repairs", "words"),
    ] {
        let context = Context::new();
        assert_eq!(
            context
                .configured(&format!("agent_tasks:\n  {key}: {value}\n"))
                .unwrap_err()
                .to_string(),
            format!("agent_tasks.{key} must be a positive integer")
        );
    }
}
