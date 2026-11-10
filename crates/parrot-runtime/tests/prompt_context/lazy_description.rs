use super::*;

#[test]
fn template_source_materialization_does_not_render() {
    let provider = TemplateSystemPromptProvider::new(
        "test:broken-template",
        "context.agent-scratch",
        vec![("path".into(), "literal {path}".into())],
        templates(
            "prompt_templates:\n  context.agent-scratch:\n    engine: minijinja\n    template: '{{ 1 / 0 }}'\n",
        ),
    );
    let mut prompt = provider.materialize(&identity()).unwrap();

    assert!(matches!(
        prompt.build(&selection()),
        Err(PromptError::Template(_))
    ));
}

#[tokio::test]
async fn raw_template_getters_return_literal_state_with_a_broken_template() {
    let provider = TemplateSystemPromptProvider::new(
        "test:broken-template",
        "context.agent-scratch",
        vec![("path".into(), "literal {path}".into())],
        templates(
            "prompt_templates:\n  context.agent-scratch:\n    engine: minijinja\n    template: '{{ 1 / 0 }}'\n",
        ),
    );
    let prompt = SessionPrompt::new(provider.materialize(&identity()).unwrap());
    let cancellation = parrot_storage::events::NeverCancelled;

    assert!(
        prompt
            .read_system_prompts(&selection(), &cancellation)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        prompt.read_prompt_templates(&cancellation).await.unwrap(),
        [(
            "test:broken-template".into(),
            "context.agent-scratch".into()
        )]
    );
    assert_eq!(
        prompt
            .read_template_arguments("test:broken-template", &cancellation)
            .await
            .unwrap(),
        [(
            "path".into(),
            serde_json::Value::String("literal {path}".into())
        )]
    );
    assert!(matches!(
        prompt.build(&selection(), &cancellation).await,
        Err(PromptError::Template(_))
    ));
}

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
