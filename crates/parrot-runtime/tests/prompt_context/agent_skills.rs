use super::*;
use parrot_core::{Skill, SkillCatalogEntry, SkillContent, SkillReference};
use parrot_domain::contracts::security::SecurityProvider;
use std::sync::atomic::AtomicUsize;

struct ClearedSkillViewPrompt {
    templates: Arc<PromptTemplateCatalog>,
}
impl SystemPrompt for ClearedSkillViewPrompt {
    fn renew_epoch(&mut self) {}
    fn read_skill_catalog(&self) -> PromptResult<Vec<SkillCatalogEntry>> {
        Err(PromptError::Context(
            "The skill catalogue has not been sampled for this epoch.".into(),
        ))
    }
    fn read_skill_catalog_live<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, PromptResult<Vec<SkillCatalogEntry>>> {
        Box::pin(async {
            Ok(vec![SkillCatalogEntry {
                name: "retained".into(),
                description: "acquired skill".into(),
                discovery_path: "/skills/retained/SKILL.md".into(),
                enabled: true,
                prompt_visible: true,
            }])
        })
    }
    fn get_template_catalog(&self, source_key: &str) -> Option<&PromptTemplateCatalog> {
        (source_key == "runtime:system-context:09b-skills").then_some(self.templates.as_ref())
    }
}

#[tokio::test]
async fn live_prompt_renders_acquired_skills_after_the_shared_view_is_cleared() {
    let prompt = SessionPrompt::new(Box::new(ClearedSkillViewPrompt {
        templates: templates(""),
    }));
    let rendered = prompt.build(&selection(), &Cancel::no()).await.unwrap();

    assert!(rendered.contains("- $retained: acquired skill (/skills/retained/SKILL.md)"));
}

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
async fn metadata_lookup_does_not_read_body_and_content_reads_keep_full_bytes_and_fresh_authority()
{
    let (reader, _, mut skills) = skills_fixture();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    let before = reader.calls.lock().unwrap().len();
    let one = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        one,
        Skill {
            id: "/skills/one/SKILL.md".into(),
            name: "one".into(),
            path: "/skills/one/SKILL.md".into(),
        }
    );
    assert_eq!(reader.calls.lock().unwrap().len(), before);
    let original = "---\nname: one\ndescription: description\n---\nbody";
    assert_eq!(
        skills
            .read_skill_content(&one, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Text(original.into())
    );
    let large = "界".repeat(3000);
    reader.file(&one.path, large.clone());
    assert_eq!(
        skills
            .read_skill_content(&one, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Text(large)
    );
    let calls = reader.calls.lock().unwrap();
    let first = &calls[before];
    let second = &calls[before + 1];
    for call in [first, second] {
        let request = call.request.as_ref().unwrap();
        assert_eq!(call.operation, "read");
        assert_eq!(request.requested.as_path(), Path::new(&one.path));
        assert_eq!(
            request.expected_physical.as_deref(),
            Some(Path::new(&one.id))
        );
        assert_eq!(request.links, LinkPolicy::Follow);
        assert_eq!(request.max_bytes, Some(1024 * 1024));
    }
    assert_ne!(first.policy, second.policy);
}

#[tokio::test]
async fn unknown_lookup_refreshes_catalog_and_keeps_the_active_snapshot_until_end_turn() {
    let (reader, catalog, mut skills) = skills_fixture();
    let before = skills.build_catalog(&Cancel::no()).await.unwrap();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    reader.skill("/skills/three/SKILL.md", "three");
    assert_eq!(skills.build_catalog(&Cancel::no()).await.unwrap(), before);
    let three = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "three".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(three.id, "/skills/three/SKILL.md");
    assert!(
        skills
            .build_catalog(&Cancel::no())
            .await
            .unwrap()
            .contains("$three:")
    );
    reader.skill("/skills/four/SKILL.md", "four");
    catalog.invalidate().await;
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
async fn content_reads_return_raw_diagnostics_and_do_not_apply_prompt_escaping_or_limits() {
    let (reader, _, mut skills) = skills_fixture();
    let one = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    let two = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "two".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    let diagnostic = "  actual\n denied <target> & details  ";
    for kind in [
        FileReadErrorKind::Denied,
        FileReadErrorKind::Io,
        FileReadErrorKind::InvalidOperation,
    ] {
        reader.error("read", &one.path, kind, diagnostic);
        assert_eq!(
            skills
                .read_skill_content(&one, &Cancel::no())
                .await
                .unwrap(),
            SkillContent::Unavailable(diagnostic.into())
        );
    }
    reader.file(&two.path, vec![0xff]);
    assert_eq!(
        skills
            .read_skill_content(&two, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Unavailable("Skill source '/skills/two/SKILL.md' is not valid UTF-8.".into())
    );
    reader.error(
        "read",
        &one.path,
        FileReadErrorKind::InvalidPath,
        "invalid read path",
    );
    assert_eq!(
        skills
            .read_skill_content(&one, &Cancel::no())
            .await
            .unwrap_err(),
        PromptError::Read(FileReadError::new(
            FileReadErrorKind::InvalidPath,
            "invalid read path"
        ))
    );
}

#[tokio::test]
async fn skill_prompt_provider_async_sampling_joins_catalog_and_cancellation_is_not_a_notice() {
    let (reader, _, mut skills) = skills_fixture();
    let one = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    *reader.cancel_at.lock().unwrap() = Some(("read", "/skills/one/SKILL.md".into()));
    assert_eq!(
        skills
            .read_skill_content(&one, &Cancel::no())
            .await
            .unwrap_err(),
        PromptError::Cancelled
    );
    reader.cancel_at.lock().unwrap().take();
    assert_eq!(
        skills
            .read_skill_content(&one, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Text("---\nname: one\ndescription: description\n---\nbody".into())
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
async fn same_prompt_product_observes_lookup_refresh_without_another_epoch_sample() {
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
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "three".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap();
    let refreshed = prompt.build(&selection).unwrap();
    assert!(refreshed.contains("$three:"));
    *reader.cancel_at.lock().unwrap() = Some(("inspect", "/skills".into()));
    assert_eq!(
        skills
            .lock()
            .await
            .get_skills(
                std::slice::from_ref(&SkillReference {
                    name: "missing".into(),
                    path: None
                }),
                &Cancel::no()
            )
            .await
            .map(|rows| rows.into_iter().next().unwrap())
            .unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(prompt.build(&selection).unwrap(), refreshed);
    skills.lock().await.end_turn();
    assert!(prompt.build(&selection).is_err());
}

#[tokio::test]
async fn catalog_template_failure_is_reported_by_build_not_lookup_or_begin_turn() {
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
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap();
    let (_, router) = crate::route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert!(matches!(
        prompt.build(&selection),
        Err(PromptError::Context(message)) if message.contains("64 KiB")
    ));
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
        "$AGENT_DIR/history.jsonl",
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
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "four".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
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
async fn linked_lookup_uses_catalog_platform_aliases_without_spurious_refresh() {
    for platform in [PathPlatform::MacOs, PathPlatform::Unix] {
        for alias in ["tmp", "var", "etc"] {
            let physical = format!("/private/{alias}/skills/review/SKILL.md");
            let root = format!("/private/{alias}/skills");
            let reader = Arc::new(Fake::default());
            reader.dir(&root);
            reader.skill(&physical, "review");
            let catalog = Arc::new(SkillCatalog::new(
                reader.clone(),
                vec![SkillRoot {
                    path: root,
                    scope: SkillScope::Repo,
                    follow_directory_links: true,
                }],
                SkillDiscoveryContext {
                    working_directory: "/private/tmp/work".into(),
                    platform,
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
            let found = skills
                .get_skills(
                    std::slice::from_ref(&SkillReference {
                        name: "review".into(),
                        path: Some(format!("/{alias}/skills/review/SKILL.md")),
                    }),
                    &Cancel::no(),
                )
                .await
                .map(|rows| rows.into_iter().next().unwrap())
                .unwrap();
            if platform == PathPlatform::MacOs {
                let found = found.unwrap();
                assert_eq!(found.id, physical);
                assert_eq!(reader.calls.lock().unwrap().len(), before);
            } else {
                assert!(found.is_none());
            }
        }
    }
}

#[tokio::test]
async fn disabled_known_lookup_does_not_refresh_and_next_enabled_duplicate_is_returned() {
    let reader = Arc::new(Fake::default());
    reader.dir("/skills");
    reader.skill("/skills/a/SKILL.md", "same");
    reader.skill("/skills/b/SKILL.md", "same");
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
        Arc::new(|| {
            (
                SkillConfiguration::new(
                    true,
                    vec![SkillConfigEntry::new("/skills/a/SKILL.md", false)],
                ),
                0,
            )
        }),
    ));
    let mut skills = AgentSkills::new(
        catalog,
        reader.clone(),
        Arc::new(Captures(AtomicUsize::new(0))),
        crate::templates(""),
        "/work".into(),
    )
    .unwrap();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    let before = reader.calls.lock().unwrap().len();
    assert_eq!(
        skills
            .get_skills(
                std::slice::from_ref(&SkillReference {
                    name: "same".into(),
                    path: None
                }),
                &Cancel::no()
            )
            .await
            .map(|rows| rows.into_iter().next().unwrap())
            .unwrap()
            .unwrap()
            .id,
        "/skills/b/SKILL.md"
    );
    assert!(
        skills
            .get_skills(
                std::slice::from_ref(&SkillReference {
                    name: "same".into(),
                    path: Some("/skills/a/SKILL.md".into())
                }),
                &Cancel::no()
            )
            .await
            .map(|rows| rows.into_iter().next().unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(reader.calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn metadata_and_content_reads_observe_prior_cancellation_without_filesystem_work() {
    let (reader, _, mut skills) = skills_fixture();
    let one = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    let before = reader.calls.lock().unwrap().len();
    let cancelled = Cancel(AtomicBool::new(true));
    assert_eq!(
        skills
            .get_skills(
                std::slice::from_ref(&SkillReference {
                    name: "one".into(),
                    path: None
                }),
                &cancelled
            )
            .await
            .map(|rows| rows.into_iter().next().unwrap())
            .unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(
        skills
            .read_skill_content(&one, &cancelled)
            .await
            .unwrap_err(),
        PromptError::Cancelled
    );
    assert_eq!(reader.calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn metadata_lookup_matches_physical_discovery_and_relative_paths_without_retargeting_content()
{
    let reader = Arc::new(Fake::default());
    reader.dir("/physical");
    reader.skill("/physical/one/SKILL.md", "one");
    reader.link("/logical", "/physical");
    let catalog = Arc::new(SkillCatalog::new(
        reader.clone(),
        vec![SkillRoot {
            path: "/logical".into(),
            scope: SkillScope::Repo,
            follow_directory_links: true,
        }],
        SkillDiscoveryContext {
            working_directory: "/working".into(),
            platform: PathPlatform::Unix,
        },
        Arc::new(|| (SkillConfiguration::default(), 0)),
    ));
    let mut skills = AgentSkills::new(
        catalog,
        reader.clone(),
        Arc::new(Captures(AtomicUsize::new(0))),
        crate::templates(""),
        "/working".into(),
    )
    .unwrap();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    let before = reader.calls.lock().unwrap().len();
    let expected = Skill {
        id: "/physical/one/SKILL.md".into(),
        name: "one".into(),
        path: "/logical/one/SKILL.md".into(),
    };
    for path in [
        "/physical/one/SKILL.md",
        "/logical/../logical/one/SKILL.md",
        "../logical/one/SKILL.md",
    ] {
        assert_eq!(
            skills
                .get_skills(
                    std::slice::from_ref(&SkillReference {
                        name: "one".into(),
                        path: Some(path.into()),
                    }),
                    &Cancel::no(),
                )
                .await
                .map(|rows| rows.into_iter().next().unwrap())
                .unwrap(),
            Some(expected.clone())
        );
    }
    assert_eq!(reader.calls.lock().unwrap().len(), before);
    reader.dir("/replacement");
    reader.skill("/replacement/one/SKILL.md", "one");
    reader.link("/logical", "/replacement");
    assert_eq!(
        skills
            .read_skill_content(&expected, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Unavailable("fixture opened target changed".into())
    );
}

#[tokio::test]
async fn authorized_content_reads_keep_the_native_one_mib_read_cap() {
    let (reader, _, mut skills) = skills_fixture();
    let skill = skills
        .get_skills(
            std::slice::from_ref(&SkillReference {
                name: "one".into(),
                path: None,
            }),
            &Cancel::no(),
        )
        .await
        .map(|rows| rows.into_iter().next().unwrap())
        .unwrap()
        .unwrap();
    reader.file(&skill.path, vec![b'x'; 1024 * 1024 + 1]);
    assert_eq!(
        skills
            .read_skill_content(&skill, &Cancel::no())
            .await
            .unwrap(),
        SkillContent::Unavailable("fixture exceeds 1 MiB limit".into())
    );
}

#[tokio::test]
async fn missing_references_refresh_catalog_once_per_input_batch() {
    let (reader, _, mut skills) = skills_fixture();
    skills.begin_turn(&Cancel::no()).await.unwrap();
    reader.calls.lock().unwrap().clear();
    let references = ["missing", "missing", "another-missing"].map(|name| SkillReference {
        name: name.into(),
        path: None,
    });
    assert_eq!(
        skills.get_skills(&references, &Cancel::no()).await.unwrap(),
        [None, None, None]
    );
    let root_scans = reader
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.operation == "list" && call.path == Path::new("/skills"))
        .count();
    assert_eq!(
        root_scans, 1,
        "missing references share one catalog refresh"
    );
    reader.skill("/skills/missing/SKILL.md", "missing");
    let next = skills.get_skills(&references, &Cancel::no()).await.unwrap();
    assert!(next[0].is_some());
    assert_eq!(next[0], next[1]);
    assert!(next[2].is_none());
}
