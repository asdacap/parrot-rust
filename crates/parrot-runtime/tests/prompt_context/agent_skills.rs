use super::*;
use parrot_domain::contracts::security::SecurityProvider;
use parrot_storage::events::ConversationPart;
use std::sync::atomic::AtomicUsize;

struct Captures(AtomicUsize);
impl SecurityProvider for Captures {
    type Snapshot = Policy;
    fn capture(&self) -> Policy {
        Policy {
            id: self.0.fetch_add(1, Ordering::SeqCst) as u32,
            denied: None,
        }
    }
}
fn skills_fixture() -> (
    Arc<Fake>,
    Arc<SkillCatalog<Fake>>,
    AgentSkills<Fake, Captures>,
) {
    let reader = Arc::new(Fake::default());
    reader.dir("/skills");
    reader.skill("/skills/one/SKILL.md", "one");
    reader.skill("/skills/two/SKILL.md", "two");
    let catalog = Arc::new(SkillCatalog::new(
        reader.clone(),
        vec![SkillRoot {
            path: "/skills".into(),
            scope: SkillScope::Repo,
            follow_directory_links: true,
        }],
        SkillDiscoveryContext {
            working_directory: "/work".into(),
            platform: PathPlatform::Unix,
        },
        Arc::new(|| (SkillConfiguration::default(), 0)),
    ));
    let state = AgentSkills::new(
        catalog.clone(),
        reader.clone(),
        Arc::new(Captures(AtomicUsize::new(0))),
        crate::templates(""),
        "/work".into(),
    )
    .unwrap();
    (reader, catalog, state)
}
#[tokio::test]
async fn selected_skills_use_actual_conversation_text_order_turn_dedup_and_full_file_bytes() {
    let (reader, _, mut skills) = skills_fixture();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    skills
        .select(
            &[
                ConversationPart::image("$one", "image/png", "$one"),
                ConversationPart::text("😀 $two"),
                ConversationPart::text("$one $two"),
            ],
            &Cancel::no(),
        )
        .await
        .unwrap();
    let rendered = skills.render(&Cancel::no()).await.unwrap();
    assert_eq!(
        rendered.loaded_paths,
        ["/skills/two/SKILL.md", "/skills/one/SKILL.md"]
    );
    assert!(
        rendered.content.find("<name>two</name>").unwrap()
            < rendered.content.find("<name>one</name>").unwrap()
    );
    assert!(
        rendered
            .content
            .contains("---\nname: two\ndescription: description\n---\nbody")
    );
    skills
        .select(&[ConversationPart::text("$two")], &Cancel::no())
        .await
        .unwrap();
    assert!(
        skills
            .render(&Cancel::no())
            .await
            .unwrap()
            .content
            .is_empty()
    );
    skills.end_turn();
    skills
        .select(&[ConversationPart::text("$two")], &Cancel::no())
        .await
        .unwrap();
    assert_eq!(
        skills.render(&Cancel::no()).await.unwrap().loaded_paths,
        ["/skills/two/SKILL.md"]
    );
    let calls = reader.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .filter_map(|call| call.request.as_ref())
            .all(|request| request.max_bytes == Some(1024 * 1024)
                && request.expected_physical.is_some())
    );
}
#[tokio::test]
async fn unknown_mentions_refresh_catalog_and_rendered_catalog_uses_actual_snapshot_identity() {
    let (reader, catalog, mut skills) = skills_fixture();
    let before = skills.build_catalog(&Cancel::no()).await.unwrap();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    reader.skill("/skills/three/SKILL.md", "three");
    assert_eq!(skills.build_catalog(&Cancel::no()).await.unwrap(), before);
    skills
        .select(&[ConversationPart::text("$three")], &Cancel::no())
        .await
        .unwrap();
    assert!(
        skills
            .build_catalog(&Cancel::no())
            .await
            .unwrap()
            .contains("$three:")
    );
    assert_eq!(
        skills.render(&Cancel::no()).await.unwrap().loaded_paths,
        ["/skills/three/SKILL.md"]
    );
    reader.skill("/skills/four/SKILL.md", "four");
    catalog.invalidate().await;
    // An active turn keeps its captured catalog even if the external catalog is invalidated.
    assert!(
        !skills
            .build_catalog(&Cancel::no())
            .await
            .unwrap()
            .contains("$four:")
    );
    skills.end_turn();
    assert!(
        skills
            .build_catalog(&Cancel::no())
            .await
            .unwrap()
            .contains("$four:")
    );
}
#[tokio::test]
async fn denied_invalid_utf8_and_too_large_skill_reads_render_notices_not_loaded_paths() {
    let (reader, _, mut skills) = skills_fixture();
    skills
        .select(&[ConversationPart::text("$one $two")], &Cancel::no())
        .await
        .unwrap();
    reader.error(
        "read",
        "/skills/one/SKILL.md",
        FileReadErrorKind::Denied,
        "  actual\n denied <target> & details  ",
    );
    reader.file("/skills/two/SKILL.md", vec![0xff]);
    let rendered = skills.render(&Cancel::no()).await.unwrap();
    assert!(rendered.loaded_paths.is_empty());
    assert!(
        rendered
            .content
            .contains("actual denied &lt;target&gt; &amp; details")
    );
    assert!(rendered.content.contains("not valid UTF-8"));
    skills.end_turn();
    reader.errors.lock().unwrap().clear();
    reader.skill("/skills/one/SKILL.md", "one");
    skills
        .select(&[ConversationPart::text("$one")], &Cancel::no())
        .await
        .unwrap();
    reader.file("/skills/one/SKILL.md", "界".repeat(3000));
    let rendered = skills.render(&Cancel::no()).await.unwrap();
    assert!(rendered.content.contains("<skill-too-large>"));
    assert!(rendered.loaded_paths.is_empty());
}
#[tokio::test]
async fn active_turn_byte_budget_reserves_summary_and_does_not_truncate_loaded_content() {
    let (reader, catalog, mut skills) = skills_fixture();
    for index in 0..150 {
        let name = format!("large-{index}");
        reader.skill(&format!("/skills/{name}/SKILL.md"), &name);
        reader.file(
            &format!("/skills/{name}/SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: description\n---\n{}",
                "x".repeat(7900)
            ),
        );
    }
    catalog.invalidate().await;
    let text = (0..150)
        .map(|index| format!("$large-{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    skills
        .select(&[ConversationPart::text(text)], &Cancel::no())
        .await
        .unwrap();
    let rendered = skills.render(&Cancel::no()).await.unwrap();
    assert!(rendered.content.len() <= MAXIMUM_TURN_BYTES);
    assert!(rendered.loaded_paths.len() < 150);
    assert!(!rendered.loaded_paths.is_empty());
    assert!(rendered.content.contains("active-turn prompt budget"));
    assert!(rendered.content.contains(&"x".repeat(7900)));
}
#[tokio::test]
async fn skill_prompt_provider_async_sampling_joins_catalog_and_cancellation_is_not_a_notice() {
    let (reader, _, mut skills) = skills_fixture();
    skills
        .select(&[ConversationPart::text("$one")], &Cancel::no())
        .await
        .unwrap();
    *reader.cancel_at.lock().unwrap() = Some(("read", "/skills/one/SKILL.md".into()));
    assert_eq!(
        skills.render(&Cancel::no()).await.unwrap_err(),
        PromptError::Cancelled
    );
    reader.cancel_at.lock().unwrap().take();
    // A failed render leaves the actual selection pending, as source RenderSelection does.
    assert_eq!(
        skills.render(&Cancel::no()).await.unwrap().loaded_paths,
        ["/skills/one/SKILL.md"]
    );
    let provider = AgentSkillPromptProvider::new(Arc::new(tokio::sync::Mutex::new(skills))).await;
    let mut prompt = provider.materialize(&crate::identity()).unwrap();
    let (_, router) = crate::route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert!(prompt.build(&selection).unwrap().contains("$one:"));
    prompt.sample_epoch(&Cancel::no()).await.unwrap();
    assert!(prompt.build(&selection).unwrap().contains("$one:"));
}

#[tokio::test]
async fn same_prompt_product_observes_select_refresh_without_another_epoch_sample() {
    let (reader, _, skills) = skills_fixture();
    let skills = Arc::new(tokio::sync::Mutex::new(skills));
    let provider = AgentSkillPromptProvider::new(skills.clone()).await;
    let mut prompt = provider.materialize(&crate::identity()).unwrap();
    let (_, router) = crate::route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert!(prompt.build(&selection).is_err());
    prompt.sample_epoch(&Cancel::no()).await.unwrap();
    assert!(!prompt.build(&selection).unwrap().contains("$three:"));
    skills.lock().await.begin_turn(&Cancel::no()).await.unwrap();
    reader.skill("/skills/three/SKILL.md", "three");
    skills
        .lock()
        .await
        .select(&[ConversationPart::text("$three")], &Cancel::no())
        .await
        .unwrap();
    let refreshed = prompt.build(&selection).unwrap();
    assert!(refreshed.contains("$three:"));
    *reader.cancel_at.lock().unwrap() = Some(("inspect", "/skills".into()));
    assert_eq!(
        skills
            .lock()
            .await
            .select(&[ConversationPart::text("$missing")], &Cancel::no())
            .await
            .unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(prompt.build(&selection).unwrap(), refreshed);
    skills.lock().await.end_turn();
    assert!(prompt.build(&selection).is_err());
}

#[tokio::test]
async fn catalog_template_failure_is_reported_by_build_not_selection_or_begin_turn() {
    let (reader, catalog, _) = skills_fixture();
    let content = format!(
        "prompt_templates:\n  context.skills:\n    template: '{}{{skills}}'\n",
        "x".repeat(MAXIMUM_CATALOG_BYTES)
    );
    let state = AgentSkills::new(
        catalog,
        reader,
        Arc::new(Captures(AtomicUsize::new(0))),
        crate::templates(&content),
        "/work".into(),
    )
    .unwrap();
    let skills = Arc::new(tokio::sync::Mutex::new(state));
    let provider = AgentSkillPromptProvider::new(skills.clone()).await;
    let mut prompt = provider.materialize(&crate::identity()).unwrap();
    skills.lock().await.begin_turn(&Cancel::no()).await.unwrap();
    skills
        .lock()
        .await
        .select(&[ConversationPart::text("$one")], &Cancel::no())
        .await
        .unwrap();
    let (_, router) = crate::route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert!(
        prompt
            .build(&selection)
            .unwrap_err()
            .to_string()
            .contains("64 KiB")
    );
}

#[tokio::test]
async fn full_runtime_prompt_joins_real_agents_skills_model_scope_and_configured_sections() {
    let (reader, _, mut skills) = skills_fixture();
    reader.file("/work/AGENTS.md", b"actual project instructions".to_vec());
    skills.begin_turn(&Cancel::no()).await.unwrap();
    let templates = crate::templates("");
    let security = Arc::new(Captures(AtomicUsize::new(0)));
    let context = MaterializedContext {
        platform: "osx-arm64".into(),
        working_directory: "/work".into(),
        scratch_path: "/session/scratch".into(),
        history_path: "/session/history.jsonl".into(),
        expected_cli_utilities: vec!["git".into()],
        optional_cli_utilities: vec!["jq".into()],
        subagents: vec![SubagentDescription {
            id: "worker".into(),
            usage: "inspect".into(),
        }],
        security: SecurityDescription {
            rules: Vec::new(),
            sandbox_enabled: true,
        },
    };
    let mut providers = materialized_context_providers(&context, templates.clone());
    providers.push(Arc::new(
        AgentsPromptProvider::new("/work", "/config", reader, security, templates.clone()).unwrap(),
    ));
    providers.push(Arc::new(
        AgentSkillPromptProvider::new(Arc::new(tokio::sync::Mutex::new(skills))).await,
    ));
    providers.push(Arc::new(StaticSystemPromptProvider::new(
        "configured:00",
        " configured bytes \n",
    )));
    let (_, router) = crate::route(Vec::new());
    providers.push(Arc::new(ModelPromptProvider::new(
        crate::profiles(&[("unused/vendor/model", None, Some("guidance"))]),
        router.clone(),
        templates.clone(),
    )));
    let scope = PromptScope::empty(templates).derive_child("planner", 1, "Review only.");
    let identity = scope
        .apply(AgentIdentityDescription {
            depth: 2,
            ..crate::identity()
        })
        .unwrap();
    let mut prompt = CompositeSystemPromptProvider::new("runtime:system", providers)
        .unwrap()
        .materialize(&identity)
        .unwrap();
    prompt.sample_epoch(&Cancel::no()).await.unwrap();
    let result = prompt
        .build(&router.resolve("unused/vendor/model").unwrap())
        .unwrap();
    assert!(result.starts_with(" configured bytes \n\n\n"));
    for text in [
        "osx-arm64",
        "actual project instructions",
        "$one:",
        "guidance",
        "1st Ancestor (planner)",
        "Review only.",
        "/work",
        "$AGENT_SCRATCH_DIR",
        "$AGENT_HISTORY_DIR/history.jsonl",
    ] {
        assert!(result.contains(text), "missing {text}");
    }
    assert!(
        result.find("osx-arm64").unwrap() < result.find("actual project instructions").unwrap()
    );
    assert!(result.find("actual project instructions").unwrap() < result.find("$one:").unwrap());
}

#[tokio::test]
async fn session_prompt_observer_then_turn_build_uses_real_skills_without_resampling_agents() {
    let (reader, catalog, skills) = skills_fixture();
    reader.file("/work/AGENTS.md", b"original epoch instructions".to_vec());
    let skills = Arc::new(tokio::sync::Mutex::new(skills));
    let providers: Vec<Arc<dyn SystemPromptProvider>> = vec![
        Arc::new(
            AgentsPromptProvider::new(
                "/work",
                "/config",
                reader.clone(),
                Arc::new(Captures(AtomicUsize::new(0))),
                crate::templates(""),
            )
            .unwrap(),
        ),
        Arc::new(AgentSkillPromptProvider::new(skills.clone()).await),
    ];
    let prompt = SessionPrompt::new(
        CompositeSystemPromptProvider::new("runtime:system", providers)
            .unwrap()
            .materialize(&crate::identity())
            .unwrap(),
    );
    let selection = crate::selection();
    let before = prompt
        .capture(&selection, &Cancel::no(), str::to_owned)
        .await
        .unwrap();
    assert!(before.contains("original epoch instructions"));
    assert!(before.contains("$one:"));
    reader.file("/work/AGENTS.md", b"next epoch instructions".to_vec());
    reader.skill("/skills/three/SKILL.md", "three");
    catalog.invalidate().await;

    // Exact drain prefix: end_turn -> ensure_epoch -> status build -> begin_turn.
    skills.lock().await.end_turn();
    prompt.ensure_epoch(&Cancel::no()).await.unwrap();
    let status = prompt.build(&selection, &Cancel::no()).await.unwrap();
    assert!(status.contains("$three:"));
    assert!(status.contains("original epoch instructions"));
    assert!(!status.contains("next epoch instructions"));
    skills.lock().await.begin_turn(&Cancel::no()).await.unwrap();
    assert_eq!(
        prompt.build(&selection, &Cancel::no()).await.unwrap(),
        status
    );
    skills.lock().await.end_turn();
    assert_eq!(
        prompt
            .capture(&selection, &Cancel::no(), str::to_owned)
            .await
            .unwrap(),
        status,
    );
    prompt.sample_epoch(&Cancel::no()).await.unwrap();
    assert!(
        prompt
            .build(&selection, &Cancel::no())
            .await
            .unwrap()
            .contains("next epoch instructions")
    );
}

#[tokio::test]
async fn live_skill_build_keeps_active_snapshot_and_captures_actual_catalog_outside_turn() {
    let (reader, catalog, skills) = skills_fixture();
    let skills = Arc::new(tokio::sync::Mutex::new(skills));
    let provider = AgentSkillPromptProvider::new(skills.clone()).await;
    let prompt = SessionPrompt::new(provider.materialize(&crate::identity()).unwrap());
    let selection = crate::selection();
    prompt
        .capture(&selection, &Cancel::no(), |_| ())
        .await
        .unwrap();
    skills.lock().await.begin_turn(&Cancel::no()).await.unwrap();
    reader.skill("/skills/three/SKILL.md", "three");
    catalog.invalidate().await;
    assert!(
        !prompt
            .build(&selection, &Cancel::no())
            .await
            .unwrap()
            .contains("$three:")
    );
    skills.lock().await.end_turn();
    assert!(
        prompt
            .capture(&selection, &Cancel::no(), str::to_owned)
            .await
            .unwrap()
            .contains("$three:")
    );
    skills.lock().await.begin_turn(&Cancel::no()).await.unwrap();
    reader.skill("/skills/four/SKILL.md", "four");
    skills
        .lock()
        .await
        .select(&[ConversationPart::text("$four")], &Cancel::no())
        .await
        .unwrap();
    assert!(
        prompt
            .build(&selection, &Cancel::no())
            .await
            .unwrap()
            .contains("$four:")
    );
}

#[tokio::test]
async fn live_skill_build_propagates_capture_cancellation_and_retries_without_stale_view() {
    let (reader, catalog, skills) = skills_fixture();
    let skills = Arc::new(tokio::sync::Mutex::new(skills));
    let provider = AgentSkillPromptProvider::new(skills.clone()).await;
    let prompt = SessionPrompt::new(provider.materialize(&crate::identity()).unwrap());
    let selection = crate::selection();
    prompt
        .capture(&selection, &Cancel::no(), |_| ())
        .await
        .unwrap();
    skills.lock().await.end_turn();
    reader.skill("/skills/three/SKILL.md", "three");
    catalog.invalidate().await;
    *reader.cancel_at.lock().unwrap() = Some(("inspect", "/skills".into()));
    assert_eq!(
        prompt
            .capture(&selection, &Cancel::no(), |_| panic!(
                "cancelled capture must not publish"
            ))
            .await
            .unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(
        prompt.build(&selection, &Cancel::no()).await.unwrap_err(),
        PromptError::Cancelled
    );
    reader.cancel_at.lock().unwrap().take();
    assert!(
        prompt
            .build(&selection, &Cancel::no())
            .await
            .unwrap()
            .contains("$three:")
    );
    let cancelled = Cancel(AtomicBool::new(true));
    assert_eq!(
        prompt.build(&selection, &cancelled).await.unwrap_err(),
        PromptError::Cancelled
    );
}

#[tokio::test]
async fn sampled_live_skill_build_can_cancel_while_waiting_for_actual_skill_owner() {
    struct Signal {
        cancelled: AtomicBool,
        notify: tokio::sync::Notify,
    }
    impl Cancellation for Signal {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let notified = self.notify.notified();
                if !self.is_cancelled() {
                    notified.await;
                }
            })
        }
    }
    let (_, _, skills) = skills_fixture();
    let skills = Arc::new(tokio::sync::Mutex::new(skills));
    let provider = AgentSkillPromptProvider::new(skills.clone()).await;
    let prompt = SessionPrompt::new(provider.materialize(&crate::identity()).unwrap());
    let selection = crate::selection();
    prompt.ensure_epoch(&Cancel::no()).await.unwrap();
    let held = skills.lock().await;
    let cancel = Signal {
        cancelled: AtomicBool::new(false),
        notify: tokio::sync::Notify::new(),
    };
    let build = prompt.build(&selection, &cancel);
    tokio::pin!(build);
    tokio::select! {
        biased;
        result = &mut build => panic!("build must await actual skill owner: {result:?}"),
        () = std::future::ready(()) => {},
    }
    cancel.cancelled.store(true, Ordering::SeqCst);
    cancel.notify.notify_waiters();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut build)
            .await
            .unwrap()
            .unwrap_err(),
        PromptError::Cancelled,
    );
    drop(held);
    assert!(
        prompt
            .build(&selection, &Cancel::no())
            .await
            .unwrap()
            .contains("$one:")
    );
}

#[tokio::test]
async fn actual_turn_linked_selection_uses_catalog_platform_aliases_without_spurious_refresh() {
    let reader = Arc::new(Fake::default());
    reader.dir("/private/tmp/skills");
    reader.skill("/private/tmp/skills/review/SKILL.md", "review");
    let catalog = Arc::new(SkillCatalog::new(
        reader.clone(),
        vec![SkillRoot {
            path: "/private/tmp/skills".into(),
            scope: SkillScope::Repo,
            follow_directory_links: true,
        }],
        SkillDiscoveryContext {
            working_directory: "/private/tmp/work".into(),
            platform: PathPlatform::MacOs,
        },
        Arc::new(|| (SkillConfiguration::default(), 0)),
    ));
    let mut skills = AgentSkills::new(
        catalog,
        reader.clone(),
        Arc::new(Captures(AtomicUsize::new(0))),
        crate::templates(""),
        "/tmp/work".into(),
    )
    .unwrap();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    let before = reader.calls.lock().unwrap().len();
    skills
        .select(
            &[ConversationPart::text(
                "[$review](/tmp/skills/review/SKILL.md)",
            )],
            &Cancel::no(),
        )
        .await
        .unwrap();
    assert_eq!(reader.calls.lock().unwrap().len(), before);
    assert_eq!(
        skills.render(&Cancel::no()).await.unwrap().loaded_paths,
        ["/private/tmp/skills/review/SKILL.md"]
    );
}
