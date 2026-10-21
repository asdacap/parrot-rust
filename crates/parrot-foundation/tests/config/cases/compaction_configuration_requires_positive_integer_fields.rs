use crate::support::Context;
#[test]
fn source_assertions() {
    for key in [
        "trigger_percent",
        "target_percent",
        "maximum_input_tokens",
        "summary_output_tokens",
    ] {
        let values: &[&str] = if key.ends_with("percent") {
            &["0", "1.5", "true", "null"]
        } else {
            &["0", "-1", "1.5", "true", "null", "2147483648"]
        };
        for value in values {
            let context = Context::new();
            assert!(
                context
                    .configured(&format!("compaction:\n  {key}: {value}\n"))
                    .is_err()
            );
        }
    }
}
