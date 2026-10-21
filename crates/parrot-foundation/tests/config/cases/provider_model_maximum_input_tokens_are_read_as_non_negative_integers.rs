use crate::support::Context;
use parrot_foundation::config::models::ModelConfigFields;
#[test]
fn source_assertions() {
    for limit in [0, 128] {
        let context = Context::new();
        let configuration = context.configured(&format!("providers:\n  custom:\n    models:\n      model:\n        max_input_tokens: {limit}\n")).unwrap();
        let model = &configuration.models.providers["custom"].models["model"];
        assert_eq!(model.max_input_tokens, limit);
        assert!(model.fields.contains(ModelConfigFields::MAX_INPUT_TOKENS));
    }
}
