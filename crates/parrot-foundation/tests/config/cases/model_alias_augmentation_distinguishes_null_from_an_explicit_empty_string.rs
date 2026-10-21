use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    let aliases = context.configured("model_aliases:\n  custom:\n    usage: Custom work\n  low_llm:\n    augment_system_prompt: \"\"\n").unwrap().models.model_aliases;
    assert_eq!(aliases["custom"].augment_system_prompt, None);
    assert_eq!(
        aliases["low_llm"].augment_system_prompt.as_deref(),
        Some("")
    );
}
