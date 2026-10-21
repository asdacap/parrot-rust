use crate::support::Context;
#[test]
fn source_assertions() {
    for value in [
        "0",
        "-1",
        "1.5",
        "true",
        "[]",
        "{}",
        "2147483648",
        "''",
        "'null'",
        "'~'",
    ] {
        for key in ["maximum_input_tokens", "summary_output_tokens"] {
            let context = Context::new();
            let failure = context
                .configured(&format!(
                    "model_profiles:\n  provider/model:\n    {key}: {value}\n"
                ))
                .unwrap_err();
            assert!(
                failure
                    .to_string()
                    .contains(&format!("model_profiles.provider/model.{key}"))
            );
        }
    }
}
