use crate::support::Context;
use parrot_domain::values::config::ModelProfileConfig;
#[test]
fn source_assertions() {
    for value in ["null", "Null", "NULL", "~", ""] {
        let context = Context::new();
        let configuration=context.configured(&format!("model_profiles:\n  provider:\n    maximum_input_tokens: 80000\n    summary_output_tokens: 14000\n  provider/model:\n    maximum_input_tokens: {value}\n    summary_output_tokens: {value}\n")).unwrap();
        assert_eq!(
            configuration
                .models
                .model_profiles
                .resolve("provider/model"),
            ModelProfileConfig::new(None, None, Some(80000), Some(14000))
        );
    }
}
