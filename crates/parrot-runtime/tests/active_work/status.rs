use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, Cancelled, Clock, OperationError},
        status::{ActiveWorkObservation, ProcessStatusSnapshot, ProcessStatusSource},
    },
    values::{
        accounting::{
            AgentSessionStatistics, AgentSessionStatisticsSnapshot, AgentUsageIncrement,
            AgentUsageKey, AgentUsageTotals,
        },
        activity::DrainState,
        context::ContextSnapshot,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::{
    active_work::{runtime_tree::*, status::*},
    agent_queues::QueueOwnerSnapshot,
    agent_turn::lifecycle::Signal,
    children::AgentIdentity,
};
use parrot_storage::{events::NeverCancelled, queue_files::QueueInfo};
use std::{
    collections::BTreeMap,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

fn catalog(content: &str) -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            content,
            &BTreeMap::from([("HOME".into(), "/synthetic-status-home".into())]),
        )
        .unwrap()
        .1,
    )
}
fn query() -> StatusQuery {
    StatusQuery {
        session_id: "session".into(),
        parent_session_id: "".into(),
        parent_session_name: "".into(),
        profile: "build".into(),
        requested_model: "provider/model".into(),
    }
}
struct FixedClock(SystemTime);
impl Clock for FixedClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        self.0
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(Cancelled)
        })
    }
}
fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock(
        SystemTime::UNIX_EPOCH + Duration::from_secs(86_400),
    ))
}
struct Scripted {
    key: String,
    calls: AtomicUsize,
    queries: Mutex<Vec<StatusQuery>>,
    available: bool,
    text: String,
}
impl Scripted {
    fn new(key: &str, text: &str) -> Arc<Self> {
        Arc::new(Self {
            key: key.into(),
            calls: AtomicUsize::new(0),
            queries: Mutex::new(Vec::new()),
            available: true,
            text: text.into(),
        })
    }
}
impl StatusProvider for Scripted {
    fn key(&self) -> &str {
        &self.key
    }
    fn observe<'a>(
        &'a self,
        query: &'a StatusQuery,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.queries.lock().unwrap().push(query.clone());
            Ok(StatusObservation {
                available: self.available,
                text: self.text.replace("{call}", &call.to_string()),
            })
        })
    }
}
#[tokio::test]
async fn observe_composes_available_status_by_key_and_resamples_providers() {
    let selection = Scripted::new("runtime:selection", "selection {call}");
    let registry = StatusRegistry::new(vec![
        selection.clone(),
        Arc::new(Scripted {
            available: false,
            ..Arc::try_unwrap(Scripted::new("runtime:unavailable", "ignored"))
                .ok()
                .unwrap()
        }),
        Scripted::new("runtime:blank", "  \t\n"),
    ])
    .unwrap();
    let profile = ProfileStatusProvider {
        key: "profile:plan".into(),
        prompt: "profile prompt".into(),
    };
    let query = StatusQuery {
        profile: "plan".into(),
        requested_model: "openai/gpt/high".into(),
        ..query()
    };
    assert_eq!(
        registry
            .observe(&query, Some(&profile), None, &NeverCancelled)
            .await
            .unwrap(),
        "profile prompt\n\nselection 1"
    );
    assert_eq!(
        registry
            .observe(&query, Some(&profile), None, &NeverCancelled)
            .await
            .unwrap(),
        "profile prompt\n\nselection 2"
    );
    assert_eq!(
        *selection.queries.lock().unwrap(),
        vec![query.clone(), query]
    );
}
#[tokio::test]
async fn register_rejects_unstable_and_duplicate_keys_including_all_per_call_extras() {
    for key in [
        "missing-namespace",
        ":missing-name",
        "missing-namespace:",
        "runtime:bad key",
        "runtime:bad\tkey",
        "runtime:bad\nkey",
        "runtime:bad\u{a0}key",
    ] {
        assert!(
            StatusRegistry::new(vec![Scripted::new(key, "x")]).is_err(),
            "{key:?}"
        );
        let registry = StatusRegistry::new(vec![]).unwrap();
        assert!(
            registry
                .observe(
                    &query(),
                    Some(Scripted::new(key, "x").as_ref()),
                    None,
                    &NeverCancelled
                )
                .await
                .is_err()
        );
        assert!(
            registry
                .observe(
                    &query(),
                    None,
                    Some(Scripted::new(key, "x").as_ref()),
                    &NeverCancelled
                )
                .await
                .is_err()
        );
    }
    let first = Scripted::new("runtime:selection", "x");
    assert!(StatusRegistry::new(vec![first.clone(), first.clone()]).is_err());
    let registry = StatusRegistry::new(vec![first.clone()]).unwrap();
    assert!(
        registry
            .observe(&query(), Some(first.as_ref()), None, &NeverCancelled)
            .await
            .is_err()
    );
    assert!(
        registry
            .observe(&query(), None, Some(first.as_ref()), &NeverCancelled)
            .await
            .is_err()
    );
    let extra = Scripted::new("extra:value", "x");
    assert!(
        registry
            .observe(
                &query(),
                Some(extra.as_ref()),
                Some(extra.as_ref()),
                &NeverCancelled
            )
            .await
            .is_err()
    );
    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        registry
            .observe(&query(), None, None, &NeverCancelled)
            .await
            .unwrap(),
        "x"
    );
}
#[tokio::test]
async fn registry_preserves_utf16_ordinal_order_and_case_sensitive_namespaces() {
    let registry = StatusRegistry::new(vec![
        Scripted::new("runtime:\u{e000}", "bmp"),
        Scripted::new("runtime:\u{10000}", "astral"),
        Scripted::new("Runtime:key", "upper"),
    ])
    .unwrap();
    assert_eq!(
        registry
            .observe(&query(), None, None, &NeverCancelled)
            .await
            .unwrap(),
        "upper\n\nastral\n\nbmp"
    );
}
struct Rendezvous {
    key: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
    finished: Arc<AtomicUsize>,
    fail: bool,
}
impl StatusProvider for Rendezvous {
    fn key(&self) -> &str {
        self.key
    }
    fn observe<'a>(
        &'a self,
        _: &'a StatusQuery,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            self.barrier.wait().await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(OperationError::Failed(StatusError::caused_by(
                    "failed",
                    std::io::Error::other("inner"),
                )))
            } else {
                Ok(StatusObservation::available_text(self.key))
            }
        })
    }
}
#[tokio::test]
async fn registry_starts_all_observations_and_joins_errors_with_provider_source() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let finished = Arc::new(AtomicUsize::new(0));
    let registry = StatusRegistry::new(vec![
        Arc::new(Rendezvous {
            key: "runtime:a",
            barrier: barrier.clone(),
            finished: finished.clone(),
            fail: true,
        }),
        Arc::new(Rendezvous {
            key: "runtime:b",
            barrier,
            finished: finished.clone(),
            fail: false,
        }),
    ])
    .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        registry.observe(&query(), None, None, &NeverCancelled),
    )
    .await
    .expect("serial observation deadlocked")
    .unwrap_err();
    let OperationError::Failed(error) = error else {
        panic!("not a keyed provider error")
    };
    assert_eq!(error.to_string(), "runtime:a: failed");
    assert!(
        error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .source()
            .unwrap()
            .is::<std::io::Error>()
    );
    assert_eq!(finished.load(Ordering::SeqCst), 2);
}
struct CancelProvider;
impl StatusProvider for CancelProvider {
    fn key(&self) -> &str {
        "runtime:cancel"
    }
    fn observe<'a>(
        &'a self,
        _: &'a StatusQuery,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(OperationError::Cancelled)
        })
    }
}
#[tokio::test]
async fn registry_propagates_actual_caller_cancellation_without_fabricating_a_section() {
    let registry = StatusRegistry::new(vec![Arc::new(CancelProvider)]).unwrap();
    let cancel = Signal::new();
    cancel.cancel();
    assert!(matches!(
        registry.observe(&query(), None, None, &cancel).await,
        Err(OperationError::Cancelled)
    ));
    let templates = catalog("");
    let providers: Vec<Box<dyn StatusProvider>> = vec![
        Box::new(ProfileStatusProvider {
            key: "profile:build".into(),
            prompt: "text".into(),
        }),
        Box::new(SelectionStatusProvider {
            templates: templates.clone(),
        }),
        Box::new(GeneratedTimeStatusProvider {
            templates: templates.clone(),
            clock: clock(),
        }),
        Box::new(ContextStatusProvider {
            templates: templates.clone(),
            snapshot: ContextSnapshot::new(1, 10, Some(10), 90),
        }),
        Box::new(StatisticsStatusProvider {
            templates,
            snapshot: AgentSessionStatisticsSnapshot::default(),
        }),
    ];
    for provider in providers {
        assert!(matches!(
            provider.observe(&query(), &cancel).await,
            Err(OperationError::Cancelled)
        ));
    }
}
#[tokio::test]
async fn context_reports_available_unavailable_and_override_windows_exactly() {
    let templates = catalog("");
    for (estimated, limit, usage, available, tokens, formatted_limit) in [
        (123, 1000, 12, true, "123", "1k"),
        (1500, 1000, 100, true, "1.5k", "1k"),
        (62539, 1050000, 5, true, "62.5k", "1.1M"),
        (123, 0, 0, false, "123", "0"),
        (123, -1, 0, false, "123", "-1"),
    ] {
        let provider = ContextStatusProvider {
            snapshot: ContextSnapshot::new(estimated, limit, available.then_some(usage), 90),
            templates: templates.clone(),
        };
        let observation = provider.observe(&query(), &NeverCancelled).await.unwrap();
        assert!(observation.available);
        let usage = if available {
            format!("{usage}% used")
        } else {
            "unavailable".into()
        };
        assert_eq!(
            observation.text,
            format!(
                "Context: {usage} ({tokens} estimated tokens / {formatted_limit} limit); reminders every 10%; automatic compaction at 90%."
            )
        );
    }
    for (trigger, expected) in [(Some(100_000), "100000"), (None, "unavailable")] {
        let mut snapshot = ContextSnapshot::new(100_000, 1_000_000, Some(10), 10);
        snapshot.input_limit = 900_000;
        snapshot.trigger_tokens = trigger;
        snapshot.has_context_limit_override = true;
        let provider = ContextStatusProvider {
            snapshot: snapshot.clone(),
            templates: templates.clone(),
        };
        assert_eq!(
            provider
                .observe(&query(), &NeverCancelled)
                .await
                .unwrap()
                .text,
            format!("Context: 10% (100k / 1M tokens); auto-compact: >{expected} tokens.")
        );
        assert!(!snapshot.exceeds_trigger());
        assert_eq!(
            snapshot.exceeds_compaction_trigger(100_001),
            trigger.is_some()
        );
        assert!(!snapshot.exceeds_input_limit());
    }
}
#[tokio::test]
async fn context_preserves_invariant_negative_metrics_and_structured_overrides() {
    for (available, custom, expected) in [
        (
            true,
            false,
            "Context: -12% used (-123 estimated tokens / 1k limit); reminders every 10%; automatic compaction at -90%.",
        ),
        (
            false,
            false,
            "Context: unavailable (-123 estimated tokens / -1 limit); reminders every 10%; automatic compaction at -90%.",
        ),
        (true, true, "-12|-123|1k|10|-90"),
        (false, true, "missing|-123|-1|10|-90"),
    ] {
        let provider = ContextStatusProvider {
            templates: catalog(if custom {
                custom_context_template()
            } else {
                ""
            }),
            snapshot: ContextSnapshot::new(
                -123,
                if available { 1000 } else { -1 },
                available.then_some(-12),
                -90,
            ),
        };
        assert_eq!(
            provider
                .observe(&query(), &NeverCancelled)
                .await
                .unwrap()
                .text,
            expected
        );
    }
}
fn custom_context_template() -> &'static str {
    "prompt_templates:\n  status.context:\n    template: '{% if available %}{{ usage }}{% else %}missing{% endif %}|{{ estimated_tokens }}|{{ context_limit }}|{{ cadence }}|{{ trigger }}'\n"
}
#[tokio::test]
async fn selection_reports_canonical_requested_selector_and_literal_parent_context() {
    let templates = catalog("");
    let provider = SelectionStatusProvider { templates };
    let mut query = query();
    query.requested_model = "provider/model/medium".into();
    assert_eq!(
        provider
            .observe(&query, &NeverCancelled)
            .await
            .unwrap()
            .text,
        "Active profile: build\nModel: provider/model/medium"
    );
    query.requested_model = "low_llm".into();
    for (parent_id, parent_name) in [
        ("", ""),
        (" \t\n", "ignored-name"),
        ("parent", ""),
        ("parent", "  "),
        ("parent", "\t\n"),
        ("parent", "main-agent"),
        (" parent ", " main-agent "),
        ("{{ hostile }}", "{name}\n{{ name }}"),
    ] {
        query.parent_session_id = parent_id.into();
        query.parent_session_name = parent_name.into();
        let parent = if parent_id.trim().is_empty() {
            String::new()
        } else if parent_name.trim().is_empty() {
            format!("\nParent session: {parent_id}")
        } else {
            format!("\nParent session: {parent_id} ({parent_name})")
        };
        assert_eq!(
            provider
                .observe(&query, &NeverCancelled)
                .await
                .unwrap()
                .text,
            format!("Active profile: build\nModel: low_llm{parent}")
        );
    }
}
#[tokio::test]
async fn selection_supports_structured_overrides_without_reparsing_literal_model() {
    let templates = catalog(
        "prompt_templates:\n  status.selection:\n    template: '{{ profile }}|{{ model }}|{% if has_parent %}{{ parent_session_id }}{% if has_parent_name %}/{{ parent_session_name }}{% endif %}{% else %}root{% endif %}'\n",
    );
    let provider = SelectionStatusProvider { templates };
    let mut query = query();
    query.requested_model = "{{ model }}".into();
    for (parent, name, expected) in [
        ("", "ignored", "build|{{ model }}|root"),
        ("parent", " \t", "build|{{ model }}|parent"),
        ("parent", " name ", "build|{{ model }}|parent/ name "),
    ] {
        query.parent_session_id = parent.into();
        query.parent_session_name = name.into();
        assert_eq!(
            provider
                .observe(&query, &NeverCancelled)
                .await
                .unwrap()
                .text,
            expected
        );
    }
}
#[tokio::test]
async fn profile_reports_only_a_nonblank_prompt() {
    for (prompt, available, expected) in [
        ("profile prompt", true, "profile prompt"),
        ("  ", false, ""),
    ] {
        let provider = ProfileStatusProvider {
            key: "profile:build".into(),
            prompt: prompt.into(),
        };
        assert_eq!(
            provider.observe(&query(), &NeverCancelled).await.unwrap(),
            StatusObservation {
                available,
                text: expected.into()
            }
        );
    }
}
#[tokio::test]
async fn generated_time_resamples_current_utc_and_preserves_seven_tick_digits() {
    for (time, expected) in [
        (
            SystemTime::UNIX_EPOCH + Duration::from_secs(86_400),
            "1970-01-02T00:00:00.0000000+00:00",
        ),
        (
            SystemTime::UNIX_EPOCH + Duration::from_nanos(123_456_789),
            "1970-01-01T00:00:00.1234567+00:00",
        ),
        (
            SystemTime::UNIX_EPOCH - Duration::from_millis(1),
            "1969-12-31T23:59:59.9990000+00:00",
        ),
        (
            SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400),
            "2000-02-29T00:00:00.0000000+00:00",
        ),
    ] {
        let provider = GeneratedTimeStatusProvider {
            templates: catalog(""),
            clock: Arc::new(FixedClock(time)),
        };
        assert_eq!(
            provider
                .observe(&query(), &NeverCancelled)
                .await
                .unwrap()
                .text,
            format!("Generated at: {expected}")
        );
    }
}
fn usage_key(effort: Option<&str>) -> AgentUsageKey {
    AgentUsageKey::from_request("provider", "model", effort.map(str::to_owned))
}
#[tokio::test]
async fn statistics_report_self_subtree_costs_efforts_and_legacy_precision() {
    let query = StatusQuery {
        session_id: "agent".into(),
        requested_model: "alias".into(),
        ..query()
    };
    for populated in [false, true] {
        let statistics = AgentSessionStatistics::default();
        if populated {
            let increment = AgentUsageIncrement {
                key: usage_key(Some("high")),
                totals: AgentUsageTotals {
                    input_tokens: 10,
                    cached_input_tokens: 4,
                    output_tokens: 2,
                    tool_calls: 1,
                    input_cost: 0.25,
                    output_cost: 0.5,
                },
                context_size: None,
                context_limit: None,
            };
            statistics.add_own(&increment).unwrap();
            statistics.add_descendant(&increment).unwrap();
            for key in [usage_key(None), AgentUsageKey::legacy()] {
                statistics
                    .add_descendant(&AgentUsageIncrement {
                        key,
                        totals: AgentUsageTotals::default(),
                        context_size: None,
                        context_limit: None,
                    })
                    .unwrap();
            }
        }
        let mut snapshot = statistics.capture();
        snapshot.has_incomplete_legacy_tool_counts = populated;
        let provider = StatisticsStatusProvider {
            templates: catalog(""),
            snapshot,
        };
        let observation = provider.observe(&query, &NeverCancelled).await.unwrap();
        assert!(observation.available);
        assert!(observation.text.starts_with("Statistics (lifetime):"));
        assert!(!observation.text.contains("alias"));
        for expected in if populated {
            vec![
                "Self: 10 (40.00% cache) / 2; 1 tool; cost $0.75 (input $0.25, output $0.50)",
                "Cumulative (self + descendants): 20 (40.00% cache) / 4; 2 tool; cost $1.50 (input $0.50, output $1.00)",
                "provider/model/high: 20 (40.00% cache)",
                "provider/model/unspecified/default:",
                "unknown/legacy/unknown (legacy):",
                "Legacy tool-execution counts may be incomplete",
            ]
        } else {
            vec![
                "Self: 0 (0.00% cache) / 0; 0 tool; cost $0.00 (input $0.00, output $0.00)",
                "Cumulative (self + descendants): 0 (0.00% cache)",
            ]
        } {
            assert!(
                observation.text.contains(expected),
                "{expected}\n{}",
                observation.text
            );
        }
        assert_eq!(observation.text.contains("may be incomplete"), populated);
        let none = observation.text.find("provider/model/unspecified/default:");
        let high = observation.text.find("provider/model/high:");
        if populated {
            assert!(none.unwrap() > high.unwrap()); /* Self high occurs before cumulative default. */
            let cumulative = observation.text.split("Cumulative").nth(1).unwrap();
            assert!(
                cumulative.find("unspecified/default").unwrap() < cumulative.find("/high").unwrap()
            );
        }
    }
}
#[tokio::test]
async fn statistics_format_source_token_rows_and_round_costs() {
    let query = StatusQuery {
        session_id: "agent".into(),
        requested_model: "alias".into(),
        ..query()
    };
    for (tokens, formatted) in [
        (999, "999"),
        (1_000, "1k"),
        (99_949, "99.9k"),
        (1_000_000, "1M"),
        (2_629_445, "2.6M"),
    ] {
        let statistics = AgentSessionStatistics::default();
        statistics
            .add_own(&AgentUsageIncrement {
                key: usage_key(Some("high")),
                totals: AgentUsageTotals {
                    input_tokens: tokens,
                    cached_input_tokens: tokens,
                    output_tokens: tokens,
                    tool_calls: 1,
                    input_cost: 3.8004980000000006,
                    output_cost: 0.4256999999999998,
                },
                context_size: None,
                context_limit: None,
            })
            .unwrap();
        let provider = StatisticsStatusProvider {
            snapshot: statistics.capture(),
            templates: catalog(""),
        };
        let text = provider
            .observe(&query, &NeverCancelled)
            .await
            .unwrap()
            .text;
        let totals = format!(
            "{formatted} (100.00% cache) / {formatted}; 1 tool; cost $4.23 (input $3.80, output $0.43)"
        );
        for prefix in [
            "Self: ",
            "Cumulative (self + descendants): ",
            "provider/model/high: ",
        ] {
            assert!(text.contains(&format!("{prefix}{totals}")), "{text}");
        }
    }
}
#[test]
fn token_formatter_retains_negative_values_and_source_custom_midpoint_rounding() {
    for (count, expected) in [
        (-123, "-123"),
        (0, "0"),
        (1250, "1.3k"),
        (1_050_000, "1.1M"),
    ] {
        assert_eq!(format_tokens(count), expected);
    }
}
struct TreeSource(Mutex<RuntimeTreeSnapshot>);
impl RuntimeTreeSnapshotSource for TreeSource {
    fn capture(&self) -> Result<RuntimeTreeSnapshot, StatusError> {
        let value = self.0.lock().unwrap();
        Ok(RuntimeTreeSnapshot {
            queue_owners: value
                .queue_owners
                .iter()
                .map(|owner| QueueOwnerSnapshot {
                    identity: owner.identity.clone(),
                    queues: owner.queues.clone(),
                })
                .collect(),
            active_agents: value.active_agents.clone(),
            processes: value.processes.clone(),
            activity: value.activity.clone(),
        })
    }
}
fn identity(id: &str, parent: &str, name: &str) -> Arc<AgentIdentity> {
    let mut identity = AgentIdentity::root(id.into(), name.into(), catalog(""));
    Arc::get_mut(&mut identity)
        .unwrap()
        .session
        .parent_session_id = parent.into();
    identity
}
fn queue(name: &str, description: &str) -> QueueInfo {
    QueueInfo {
        path: "synthetic-not-read".into(),
        name: name.into(),
        description: description.into(),
        size: 0,
        closed: false,
    }
}
fn process(owner: &str, process_id: &str, name: &str) -> ProcessStatusSnapshot {
    ProcessStatusSnapshot {
        owner_session_id: owner.into(),
        process_id: process_id.into(),
        name: name.into(),
        state: parrot_domain::contracts::status::ActiveWorkState::Running,
    }
}
#[tokio::test]
async fn runtime_tree_nests_queues_processes_active_agents_and_preserves_source_sorting() {
    let child = identity("child", "root", "worker");
    let source = Arc::new(TreeSource(Mutex::new(RuntimeTreeSnapshot {
        queue_owners: vec![
            QueueOwnerSnapshot {
                identity: identity("root", "", "main"),
                queues: vec![queue("work", "queued work\n{{ hostile }}")],
            },
            QueueOwnerSnapshot {
                identity: child.clone(),
                queues: vec![queue("results", "")],
            },
        ],
        active_agents: vec![child.session.clone()],
        processes: vec![
            process("child", "opaque-1", "fetch"),
            process("root", "opaque-2", "build"),
        ],
        activity: vec![("child".into(), DrainState::Running)],
    })));
    let provider = RuntimeTreeStatusProvider {
        source: source.clone(),
        templates: catalog(""),
    };
    let mut query = query();
    query.session_id = "root".into();
    assert_eq!(
        provider
            .observe(&query, &NeverCancelled)
            .await
            .unwrap()
            .text,
        "Runtime:\n- agent: main\n  - queue: work (0 items, description: \"queued work\\n{{ hostile }}\")\n  - process: root/build (shell, running, name: build)\n  - agent: worker (running)\n    - queue: results (0 items)\n    - process: child/fetch (shell, running, name: fetch)"
    );
    // A child query must not disclose its parent or unrelated roots.
    query.session_id = "child".into();
    let text = provider
        .observe(&query, &NeverCancelled)
        .await
        .unwrap()
        .text;
    assert!(text.starts_with("Runtime:\n- agent: worker (running)"));
    assert!(!text.contains("main"));
    assert!(!text.contains("root/build"));
    source.0.lock().unwrap().processes =
        vec![process("root", "a", "zulu"), process("root", "z", "alpha")];
    source.0.lock().unwrap().queue_owners[0].queues = vec![queue("zulu", ""), queue("alpha", "")];
    query.session_id = "root".into();
    let text = provider
        .observe(&query, &NeverCancelled)
        .await
        .unwrap()
        .text;
    assert!(text.find("queue: alpha").unwrap() < text.find("queue: zulu").unwrap());
    assert!(text.find("root/zulu").unwrap() < text.find("root/alpha").unwrap());
}
#[tokio::test]
async fn runtime_tree_reports_idle_root_custom_model_cycles_and_unknown_owners_without_leaks() {
    let source = Arc::new(TreeSource(Mutex::new(RuntimeTreeSnapshot {
        queue_owners: vec![QueueOwnerSnapshot {
            identity: identity("root", "", "main"),
            queues: vec![],
        }],
        ..RuntimeTreeSnapshot::default()
    })));
    let mut query = query();
    query.session_id = "root".into();
    for (custom, expected) in [(false, "Runtime:\n- agent: main"), (true, "tree:main;")] {
        let templates = catalog(if custom {
            "prompt_templates:\n  status.runtime:\n    template: '{{ section }}:{% for agent in agents %}{{ agent.name }};{% endfor %}'\n"
        } else {
            ""
        });
        let provider = RuntimeTreeStatusProvider {
            source: source.clone(),
            templates,
        };
        assert_eq!(provider.key(), "runtime:queues");
        let observation = provider.observe(&query, &NeverCancelled).await.unwrap();
        assert!(observation.available);
        assert_eq!(observation.text, expected);
    }
    let child = identity("child", "root", "worker");
    {
        let mut snapshot = source.0.lock().unwrap();
        snapshot.queue_owners[0].identity = identity("root", "child", "main");
        snapshot.active_agents.push(child.session.clone());
        snapshot
            .processes
            .push(process("unrelated", "pid", "secret"));
    }
    let provider = RuntimeTreeStatusProvider {
        source,
        templates: catalog(""),
    };
    let text = provider
        .observe(&query, &NeverCancelled)
        .await
        .unwrap()
        .text;
    assert_eq!(text, "Runtime:\n- agent: main\n  - agent: worker");
}
#[tokio::test]
async fn runtime_status_preserves_additional_context_order_exclusions_and_per_call_isolation() {
    let runtime = RuntimeStatus::new(
        catalog(""),
        clock(),
        vec![Scripted::new("runtime:queues", "Runtime:\n- agent: main")],
    )
    .unwrap();
    let query = query();
    let full = runtime
        .observe_with_context(
            &query,
            "profile prompt",
            ContextSnapshot::new(123, 1000, Some(12), 90),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(full.starts_with("profile prompt\n\nGenerated at: 1970-01-02T00:00:00.0000000+00:00"));
    assert!(full.contains("\n\nRuntime:\n- agent: main\n\nContext: 12% used"));
    assert!(full.ends_with("\n\nActive profile: build\nModel: provider/model"));
    let (first, second) = tokio::join!(
        runtime.observe_runtime(
            &query,
            ContextSnapshot::new(100, 1000, Some(10), 90),
            &NeverCancelled
        ),
        runtime.observe_runtime(
            &query,
            ContextSnapshot::new(800, 1000, Some(80), 90),
            &NeverCancelled
        )
    );
    for (text, present, absent) in [
        (first.unwrap(), "10% used", "80% used"),
        (second.unwrap(), "80% used", "10% used"),
    ] {
        assert!(text.contains(present));
        assert!(!text.contains(absent));
        for excluded in [
            "profile prompt",
            "Active profile:",
            "Model:",
            "Generated at:",
        ] {
            assert!(!text.contains(excluded));
        }
    }
    let statistics = runtime
        .observe_statistics(
            &query,
            AgentSessionStatisticsSnapshot::default(),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(statistics.starts_with("Statistics (lifetime):"));
    assert!(!statistics.contains("Context:"));
    assert!(!statistics.contains("Runtime:"));
    assert!(!statistics.contains("provider/model"));
}
#[tokio::test]
async fn fitted_status_converges_with_full_snapshot_equality_and_caps_at_eight_rerenders() {
    let observed = Scripted::new("runtime:value", "constant");
    let runtime = RuntimeStatus::new(catalog(""), clock(), vec![observed.clone()]).unwrap();
    let mut calls = Vec::new();
    let result = runtime
        .observe_fitted(
            &query(),
            "prompt",
            |text| {
                calls.push(text.to_owned());
                Ok(ContextSnapshot::new(
                    if text.is_empty() { 0 } else { 100 },
                    1000,
                    Some(10),
                    90,
                ))
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], "");
    assert!(result.contains("100 estimated tokens"));
    assert_eq!(observed.calls.load(Ordering::SeqCst), 2);
    let mut estimates = 0;
    runtime
        .observe_fitted(
            &query(),
            "prompt",
            |_| {
                estimates += 1;
                Ok(ContextSnapshot::new(estimates, 1000, Some(10), 90))
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(estimates, 9);
    assert_eq!(observed.calls.load(Ordering::SeqCst), 11);
    // Same visible metrics, different input limit: source record equality still rerenders.
    let before = observed.calls.load(Ordering::SeqCst);
    let mut estimates = 0;
    runtime
        .observe_fitted(
            &query(),
            "prompt",
            |_| {
                estimates += 1;
                let mut context = ContextSnapshot::new(1, 1000, Some(10), 90);
                context.input_limit = if estimates == 1 { 1000 } else { 500 };
                Ok(context)
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(estimates, 3);
    assert_eq!(observed.calls.load(Ordering::SeqCst) - before, 2);
}
#[tokio::test]
async fn fitted_status_propagates_estimation_and_render_failures() {
    let runtime = RuntimeStatus::new(catalog(""), clock(), vec![]).unwrap();
    let result = runtime
        .observe_fitted(
            &query(),
            "prompt",
            |_| Err(StatusError::new("estimate failed")),
            &NeverCancelled,
        )
        .await;
    assert_eq!(result.unwrap_err().to_string(), "estimate failed");
    let mut bad = query();
    bad.profile = "bad profile".into();
    assert!(
        runtime
            .observe_with_context(
                &bad,
                "text",
                ContextSnapshot::new(1, 10, Some(10), 90),
                &NeverCancelled
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("profile provider requires a stable namespaced key")
    );
}

#[tokio::test]
async fn bound_context_status_uses_actual_full_status_and_preserves_snapshot_and_cancellation() {
    use parrot_runtime::compaction::ContextError;
    let runtime = Arc::new(
        RuntimeStatus::new(
            catalog(""),
            clock(),
            vec![Scripted::new("runtime:queues", "Runtime:\n- agent: main")],
        )
        .unwrap(),
    );
    let bound = runtime.bind_context(query(), "profile prompt".into());
    let first = bound
        .observe(
            &ContextSnapshot::new(100, 1000, Some(10), 90),
            &NeverCancelled,
        )
        .await
        .unwrap();
    let second = bound
        .observe(
            &ContextSnapshot::new(800, 1000, Some(80), 90),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(first.starts_with("profile prompt\n\nGenerated at: "));
    assert!(first.contains("Runtime:\n- agent: main\n\nContext: 10% used"));
    assert!(first.ends_with("Active profile: build\nModel: provider/model"));
    assert!(!first.contains("80% used"));
    assert!(second.contains("80% used"));
    assert!(!second.contains("10% used"));
    let estimates = AtomicUsize::new(0);
    let fitted = bound
        .observe_fitted(
            &|text| {
                estimates.fetch_add(1, Ordering::SeqCst);
                Ok(ContextSnapshot::new(
                    if text.is_empty() { 0 } else { 100 },
                    1000,
                    Some(10),
                    90,
                ))
            },
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(fitted.contains("100 estimated tokens"));
    assert_eq!(estimates.load(Ordering::SeqCst), 3);
    assert!(
        matches!(bound.observe_fitted(&|_| Err(ContextError::Argument("original estimate".into())), &NeverCancelled).await, Err(ContextError::Argument(message)) if message == "original estimate")
    );
    assert!(matches!(
        bound
            .observe_fitted(&|_| Err(ContextError::Cancelled), &NeverCancelled)
            .await,
        Err(ContextError::Cancelled)
    ));
    let cancelled = Signal::new();
    cancelled.cancel();
    assert!(matches!(
        bound
            .observe(&ContextSnapshot::new(1, 10, Some(10), 90), &cancelled)
            .await,
        Err(ContextError::Cancelled)
    ));
    let runtime_only = runtime
        .observe_runtime(
            &query(),
            ContextSnapshot::new(1, 10, Some(10), 90),
            &NeverCancelled,
        )
        .await
        .unwrap();
    for excluded in [
        "profile prompt",
        "Generated at:",
        "Active profile:",
        "Model:",
    ] {
        assert!(!runtime_only.contains(excluded));
    }
}

struct ImmediateFailure {
    cancelled: bool,
}
impl StatusProvider for ImmediateFailure {
    fn key(&self) -> &str {
        if self.cancelled {
            "runtime:a-cancel"
        } else {
            "runtime:z-failure"
        }
    }
    fn observe<'a>(
        &'a self,
        _: &'a StatusQuery,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, StatusResult<StatusObservation>> {
        Box::pin(async move {
            if self.cancelled {
                Err(OperationError::Cancelled)
            } else {
                Err(OperationError::Failed(StatusError::new("failed")))
            }
        })
    }
}
#[tokio::test]
async fn registry_wraps_foreign_cancellation_and_faults_take_precedence_after_join() {
    let registry =
        StatusRegistry::new(vec![Arc::new(ImmediateFailure { cancelled: true })]).unwrap();
    assert_eq!(
        registry
            .observe(&query(), None, None, &NeverCancelled)
            .await
            .unwrap_err()
            .to_string(),
        "runtime:a-cancel: operation cancelled"
    );
    let registry = StatusRegistry::new(vec![
        Arc::new(ImmediateFailure { cancelled: true }),
        Arc::new(ImmediateFailure { cancelled: false }),
    ])
    .unwrap();
    let cancelled = Signal::new();
    cancelled.cancel();
    assert_eq!(
        registry
            .observe(&query(), None, None, &cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "runtime:z-failure: failed"
    );
}

// Reuse the accepted real runtime/session/history fixture, not a fake registry.
use super::support;

#[derive(Default)]
struct ProcessSource {
    snapshots: Mutex<Vec<ProcessStatusSnapshot>>,
    blocking_ids: Mutex<Vec<String>>,
}
impl ProcessStatusSource for ProcessSource {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        let blocking = self.blocking_ids.lock().unwrap();
        self.snapshots
            .lock()
            .unwrap()
            .iter()
            .filter(|process| blocking.contains(&process.process_id))
            .map(|process| ActiveWorkObservation {
                id: process.id(),
                name: process.name.clone(),
                state: process.state.clone(),
            })
            .collect()
    }
    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        self.snapshots.lock().unwrap().clone()
    }
}
struct QueueDiagnostics;
impl parrot_runtime::agent_queues::QueueDiagnostics for QueueDiagnostics {
    fn record(&self, _: parrot_runtime::agent_queues::QueueDiagnostic) {}
}
struct RealFactory {
    sessions: Arc<support::Factory>,
    resources: parrot_foundation::paths::UserSessionResources<()>,
    processes: Mutex<BTreeMap<String, Arc<ProcessSource>>>,
    omit_process: bool,
}
impl parrot_runtime::children::ScopeFactory<support::Security> for RealFactory {
    fn create(
        &self,
        prepared: parrot_runtime::children::PreparedScope<support::Security>,
    ) -> BoxFuture<
        '_,
        parrot_runtime::children::ChildResult<
            parrot_runtime::children::ScopeParts<support::Security>,
        >,
    > {
        Box::pin(async move {
            use parrot_runtime::{
                agent_queues::{AgentQueues, QueueInventory},
                children::ServiceRegistration,
            };
            let identity = prepared.identity.clone();
            let inventory = QueueInventory::new(
                parrot_storage::queue_files::Owner {
                    session_id: identity.session.session_id.clone(),
                    name: identity.session.name.clone(),
                    parent_session_id: identity.session.parent_session_id.clone(),
                    parent_name: identity.parent_name.clone(),
                },
                support::id(),
            )
            .unwrap();
            let queues = AgentQueues::<support::Security>::new(
                identity.clone(),
                &self.resources,
                inventory,
                Arc::new(QueueDiagnostics),
            )
            .unwrap();
            queues.initialize().unwrap();
            let mut parts = self.sessions.build(prepared).await?;
            parts
                .services
                .push(ServiceRegistration::new(queues.clone()));
            parts.final_resources.push(queues);
            if !self.omit_process {
                let processes = Arc::new(ProcessSource::default());
                self.processes
                    .lock()
                    .unwrap()
                    .insert(identity.session.name.clone(), processes.clone());
                let service: Arc<dyn ProcessStatusSource> = processes;
                parts.services.push(ServiceRegistration::new(service));
            }
            Ok(parts)
        })
    }
}
async fn real_root(
    omit_process: bool,
) -> (
    Arc<parrot_runtime::children::AgentRegistry<support::Security>>,
    Arc<RealFactory>,
    Arc<parrot_runtime::children::Scope<support::Security>>,
    Arc<support::Histories>,
) {
    use parrot_runtime::children::*;
    let histories = support::Histories::new();
    let sessions = Arc::new(support::Factory::default());
    *sessions.histories.lock().unwrap() = Some(histories.clone());
    let resources = parrot_foundation::paths::UserSessionResources::new(
        histories
            .repository
            .events()
            .database()
            .selected_root()
            .clone(),
        parrot_domain::values::identifiers::UserSessionId::parse(
            histories.path.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap(),
        (),
    )
    .unwrap();
    let factory = Arc::new(RealFactory {
        sessions,
        resources,
        processes: Mutex::new(BTreeMap::new()),
        omit_process,
    });
    let registry = AgentRegistry::new(RegistryConfig {
        agent_limits: parrot_foundation::config::runtime::AgentLimitsConfig {
            disable_recursion_limit: false,
            ..Default::default()
        },
        profiles: Arc::new(support::Profiles),
        factory: factory.clone(),
        histories: histories.clone(),
        ids: Arc::new(support::Ids),
        diagnostics: Arc::new(support::Diagnostics::default()),
        retained: parrot_domain::values::accounting::RetainedAgentBudget::new(8),
        lifetime: Arc::new(Signal::new()),
        validate_child: Arc::new(
            parrot_runtime::agent_queues::QueueChildAdmissionValidator::validate,
        ),
    });
    let identity = AgentIdentity::root(support::id(), "root".into(), catalog(""));
    let history = histories.history(&identity);
    let selection = Arc::new(SelectionCell::new(
        support::selection(&support::Scripted::default(), "first", "root", 0xff),
        PolicyLineage::root(),
    ));
    struct NoParent;
    impl parrot_runtime::agent_turn::services::ParentCompletion for NoParent {
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a parrot_runtime::agent_turn::lifecycle::Execution,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), parrot_runtime::agent_turn::services::TurnError>,
    > {
        self.deliver(execution)
    }

        fn deliver<'a>(
            &'a self,
            _: &'a parrot_runtime::agent_turn::lifecycle::Execution,
        ) -> BoxFuture<'a, Result<(), parrot_runtime::agent_turn::services::TurnError>> {
            Box::pin(async { Ok(()) })
        }
    }
    let parts = parrot_runtime::children::ScopeFactory::create(
        factory.as_ref(),
        PreparedScope {
            identity: identity.clone(),
            history: history.clone(),
            selection: selection.clone(),
            lineage: PolicyLineage::root(),
            parent_completion: Arc::new(NoParent),
            lifetime: registry.lifetime(),
        },
    )
    .await
    .unwrap();
    let scope = Scope::create(ScopeConfig {
        authority: registry.clone(),
        identity,
        history,
        selection,
        parent: None,
        delivery: CompletionDelivery::RetainedOnly,
        retention: None,
        parts,
    })
    .join()
    .await
    .unwrap();
    scope
        .get_service::<parrot_runtime::agent_queues::AgentQueues<support::Security>>()
        .unwrap()
        .bind_scope(&scope)
        .unwrap();
    registry.register_root(scope.clone()).unwrap();
    (registry, factory, scope, histories)
}
#[tokio::test]
async fn registry_runtime_tree_source_reads_actual_retained_scopes_queues_and_process_port() {
    use parrot_runtime::{agent_queues::AgentQueues, children::*};
    support::bounded(async {
        let (registry, factory, root, _histories) = real_root(false).await;
        let root_id = root.identity().session.session_id.clone();
        root.get_service::<AgentQueues<support::Security>>().unwrap().create("work", "queued work").unwrap();
        let request = LaunchRequest { parent: root.session().clone(), requested_name: "worker".into(), requested_profile: "child".into(), model: support::Scripted::default().selection("first"), requested_scope: "status worker".into(), delivery: CompletionDelivery::RetainedOnly, boundary: parrot_storage::conversation::HistoryForkBoundary::AfterCompletedHistory, fork: parrot_storage::conversation::HistoryForkSelection::parse("empty"), history_source: HistorySource::Parent };
        let child = root.spawner().spawn(request).unwrap().join().await.unwrap();
        let child_id = child.identity().session.session_id.clone();
        child.get_service::<AgentQueues<support::Security>>().unwrap().create("results", "").unwrap();
        factory.processes.lock().unwrap().get("root").unwrap().snapshots.lock().unwrap().push(process(&root_id, "opaque-z", "build"));
        factory.processes.lock().unwrap().get("worker").unwrap().snapshots.lock().unwrap().push(process(&child_id, "opaque-a", "fetch"));
        // Completed-but-undelivered snapshots remain visible as Running even
        // when Active() does not classify them as completion blockers.
        assert!(root.get_service::<dyn ProcessStatusSource>().unwrap().active().is_empty());
        assert!(child.get_service::<dyn ProcessStatusSource>().unwrap().active().is_empty());
        let gate = support::Gate::new(); let provider = support::Scripted::default(); provider.gates.lock().unwrap().push_back(gate.clone());
        // Child selection captures this controlled provider; the session/drain is real.
        child.session().update_selection(support::selection(&provider, "first", "child", 0xff)).unwrap();
        let sent = support::send(child.session().as_ref(), "work").await; gate.entered().await;
        let sessions = factory.sessions.clone();
        let source = RegistryRuntimeTreeSource::new(registry.clone(), Arc::new(move |scope| Ok(sessions.get(&scope.identity().session.name).adapter.session().state())));
        let captured = source.capture().unwrap(); assert_eq!(captured.queue_owners.len(), 2); assert_eq!(captured.active_agents.len(), 1); assert_eq!(captured.active_agents[0].session_id, child_id); assert_eq!(captured.processes.len(), 2);
        let provider = RuntimeTreeStatusProvider { source: Arc::new(source), templates: catalog("") };
        let mut query = query(); query.session_id = root_id.clone();
        assert_eq!(provider.observe(&query, &NeverCancelled).await.unwrap().text, format!("Runtime:\n- agent: root\n  - queue: work (0 items, description: \"queued work\")\n  - process: {root_id}/build (shell, running, name: build)\n  - agent: worker (running)\n    - queue: results (0 items)\n    - process: {child_id}/fetch (shell, running, name: fetch)"));
        gate.open(); sent.execution.join().await.unwrap();
        registry.shutdown().join().await.unwrap();
    }).await;
}
#[tokio::test]
async fn registry_runtime_tree_source_missing_process_service_is_error_not_silent_omission() {
    support::bounded(async {
        let (registry, _factory, _root, _histories) = real_root(true).await;
        let source =
            RegistryRuntimeTreeSource::new(registry.clone(), Arc::new(|_| Ok(DrainState::Idle)));
        assert!(
            source
                .capture()
                .err()
                .unwrap()
                .to_string()
                .contains("service")
        );
        registry.shutdown().join().await.unwrap();
    })
    .await;
}
