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
async fn system_prompt_reads_render_provider_templates_without_reinterpreting_arguments() {
    let catalog = templates(
        "prompt_templates:\n  custom.list:\n    engine: minijinja\n    template: '{{ title }}{% for item in items %}[{{ item }}]{% endfor %}'\n    allowed_arguments: [title, items]\n    required_arguments: [title, items]\n  custom.scalar:\n    template: 'configured {content}'\n    allowed_arguments: [content]\n    required_arguments: [content]\n",
    );
    let provider = CompositeSystemPromptProvider::new(
        "test:root",
        vec![
            Arc::new(TemplateSystemPromptProvider::new(
                "test:20-scalar",
                "custom.scalar",
                vec![("content".into(), "opaque {title}".into())],
                catalog.clone(),
            )),
            Arc::new(TemplateSystemPromptProvider::new(
                "test:10-list",
                "custom.list",
                vec![
                    ("title".into(), "literal {{ untouched }}".into()),
                    ("items".into(), serde_json::json!(["alpha", "beta"])),
                ],
                catalog,
            )),
        ],
    )
    .unwrap();
    let prompt = SessionPrompt::new(provider.materialize(&identity()).unwrap());
    let cancellation = parrot_storage::events::NeverCancelled;
    prompt.ensure_epoch(&cancellation).await.unwrap();

    assert_eq!(
        prompt
            .read_system_prompts(&selection(), &cancellation)
            .await
            .unwrap(),
        [
            (
                "test:10-list".into(),
                "literal {{ untouched }}[alpha][beta]".into(),
            ),
            ("test:20-scalar".into(), "configured opaque {title}".into()),
        ]
    );
    assert_eq!(
        prompt.build(&selection(), &cancellation).await.unwrap(),
        "literal {{ untouched }}[alpha][beta]\n\nconfigured opaque {title}"
    );
}

#[tokio::test]
async fn provider_template_failures_surface_when_reading_system_prompts() {
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

    prompt.ensure_epoch(&cancellation).await.unwrap();
    assert!(matches!(
        prompt
            .read_system_prompts(&selection(), &cancellation)
            .await,
        Err(PromptError::Template(_))
    ));
    assert!(matches!(
        prompt.build(&selection(), &cancellation).await,
        Err(PromptError::Template(_))
    ));
}

#[tokio::test]
async fn system_prompt_reads_include_complete_model_guidance_and_literal_augmentation() {
    for guidance in ["model guidance", "\t"] {
        for augmentation in ["literal {{ alias }}\n\ttext", "  ", ""] {
            let mut alias =
                ModelAliasDefinition::new("preferred", "unused/vendor/model", "primary");
            alias.augment_system_prompt = Some(augmentation.into());
            let (_, router) = route(vec![alias]);
            let prompt = SessionPrompt::new(
                ModelPromptProvider::new(
                    profiles(&[("unused/vendor/model", Some("profile usage"), Some(guidance))]),
                    router.clone(),
                    templates(
                        "prompt_templates:\n  context.model-aliases:\n    engine: minijinja\n    template: '{% for alias in aliases %}{{ alias.name }}={{ alias.model }} [{{ alias.usage }}]{% endfor %}'\n",
                    ),
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
            let base =
                format!("preferred=unused/vendor/model [primary\nprofile usage]\n\n{guidance}");
            let expected = if augmentation.is_empty() {
                base.clone()
            } else {
                format!("{base}\n\n{augmentation}")
            };
            assert_eq!(
                sections,
                [(
                    "runtime:system-context:10-model-prompt".into(),
                    expected.clone()
                )]
            );
            assert_eq!(
                prompt
                    .read_system_prompts(
                        &router.resolve("unused/vendor/model").unwrap(),
                        &cancellation,
                    )
                    .await
                    .unwrap(),
                [("runtime:system-context:10-model-prompt".into(), base)]
            );
            assert_eq!(
                prompt.build(&selected, &cancellation).await.unwrap(),
                expected
            );
        }
    }
}

#[tokio::test]
async fn system_prompt_reads_include_model_guidance_without_aliases() {
    let (_, router) = route(Vec::new());
    let prompt = SessionPrompt::new(
        ModelPromptProvider::new(
            profiles(&[("unused/vendor/model", None, Some("model guidance"))]),
            router.clone(),
            templates(""),
        )
        .materialize(&identity())
        .unwrap(),
    );
    assert_eq!(
        prompt
            .read_system_prompts(
                &router.resolve("unused/vendor/model").unwrap(),
                &parrot_storage::events::NeverCancelled,
            )
            .await
            .unwrap(),
        [(
            "runtime:system-context:10-model-prompt".into(),
            "model guidance".into()
        )]
    );
}

#[tokio::test]
async fn model_template_failures_surface_when_reading_system_prompts() {
    let (_, router) = route(vec![ModelAliasDefinition::new(
        "preferred",
        "unused/vendor/model",
        "primary",
    )]);
    let prompt = SessionPrompt::new(
        ModelPromptProvider::new(
            profiles(&[]),
            router.clone(),
            templates(
                "prompt_templates:\n  context.model-aliases:\n    engine: minijinja\n    template: '{{ 1 / 0 }}'\n",
            ),
        )
        .materialize(&identity())
        .unwrap(),
    );
    let cancellation = parrot_storage::events::NeverCancelled;
    prompt.ensure_epoch(&cancellation).await.unwrap();
    assert!(matches!(
        prompt
            .read_system_prompts(&router.resolve("preferred").unwrap(), &cancellation)
            .await,
        Err(PromptError::Template(_))
    ));
}

#[tokio::test]
async fn system_prompt_reads_support_custom_template_ids() {
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
    assert_eq!(
        prompt
            .read_system_prompts(&selection(), &cancellation)
            .await
            .unwrap(),
        [("test:custom-template".into(), "custom section".into())]
    );
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
