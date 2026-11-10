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
    prompt
        .read_core_prompt_bindings(&cancellation)
        .await
        .unwrap();
    assert!(matches!(
        prompt.build(&selection(), &cancellation).await,
        Err(PromptError::Template(_))
    ));
}

#[tokio::test]
async fn raw_system_prompts_include_only_the_selected_alias_augmentation_verbatim() {
    let mut alias = ModelAliasDefinition::new("preferred", "unused/vendor/model", "primary");
    alias.augment_system_prompt = Some("literal {{ alias }}\n\ttext".into());
    let (_, router) = route(vec![alias]);
    let prompt = SessionPrompt::new(
        ModelPromptProvider::new(
            profiles(&[("unused/vendor/model", None, Some("model guidance"))]),
            router.clone(),
            templates(""),
        )
        .materialize(&identity())
        .unwrap(),
    );
    let cancellation = parrot_storage::events::NeverCancelled;
    let selected = router.resolve("preferred").unwrap();
    let sections = prompt
        .read_system_prompts(&selected, &cancellation)
        .await
        .unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].1, "literal {{ alias }}\n\ttext");
    assert!(
        prompt
            .read_system_prompts(
                &router.resolve("unused/vendor/model").unwrap(),
                &cancellation,
            )
            .await
            .unwrap()
            .is_empty()
    );
    let rendered = prompt.build(&selected, &cancellation).await.unwrap();
    assert!(rendered.ends_with("model guidance\n\nliteral {{ alias }}\n\ttext"));
    assert_eq!(rendered.matches("literal {{ alias }}").count(), 1);
}

#[tokio::test]
async fn core_bindings_reject_configured_templates_without_an_explicit_key() {
    let provider = TemplateSystemPromptProvider::new(
        "test:custom-template",
        "custom.section",
        Vec::new(),
        templates(
            "prompt_templates:\n  custom.section:\n    template: 'custom section'\n    allowed_arguments: []\n    required_arguments: []\n",
        ),
    );
    let prompt = SessionPrompt::new(provider.materialize(&identity()).unwrap());
    let cancellation = parrot_storage::events::NeverCancelled;

    assert_eq!(
        prompt.build(&selection(), &cancellation).await.unwrap(),
        "custom section"
    );
    let error = prompt
        .read_core_prompt_bindings(&cancellation)
        .await
        .unwrap_err();
    assert!(matches!(error, PromptError::Template(message) if message.contains("custom.section")));
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
