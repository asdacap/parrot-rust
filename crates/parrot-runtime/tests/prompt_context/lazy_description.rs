use super::*;

#[test]
fn security_description_template_is_lazy_and_retains_its_first_error_across_epochs() {
    let catalog =
        templates("prompt_templates:\n  context.security-profile:\n    template: '{{ 1 / 0 }}'\n");
    let providers = materialized_context_providers(&context(), catalog);
    let description = providers
        .iter()
        .find(|provider| provider.key().ends_with(":16-security-profile"))
        .unwrap();
    // This is rendering of an inert supplied description, not a SecurityProfile service.
    let mut prompt = description.materialize(&identity()).unwrap();
    let first = prompt.build(&selection()).unwrap_err();
    prompt.renew_epoch();
    assert_eq!(prompt.build(&selection()).unwrap_err(), first);
}

#[test]
fn security_description_samples_sandbox_switch_at_agent_materialization() {
    use std::sync::atomic::AtomicBool;

    let enabled = Arc::new(AtomicBool::new(false));
    let current = enabled.clone();
    let provider = SecurityProfilePromptProvider::new(
        context().security.rules,
        Arc::new(move || current.load(Ordering::Acquire)),
        templates(""),
    );
    let mut disabled = provider.materialize(&identity()).unwrap();
    enabled.store(true, Ordering::Release);
    let mut active = provider.materialize(&identity()).unwrap();
    let first = disabled.build(&selection()).unwrap();
    assert!(first.contains("The OS process sandbox is currently disabled"));
    assert!(
        !active
            .build(&selection())
            .unwrap()
            .contains("currently disabled")
    );
    disabled.renew_epoch();
    assert_eq!(disabled.build(&selection()).unwrap(), first);
}
