//! User-session usage projection, seeded from durable replay and published only
//! after a newer usage fact has been durably appended.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::{Arc, Mutex};

use parrot_domain::protocol::messages::parrot::{
    AgentStatisticsUpdatedEvent, Event, SessionUsageSnapshot,
};
use parrot_domain::values::accounting::{
    AgentSessionStatistics, AgentSessionStatisticsSnapshot, AgentUsageIncrement,
};

/// Replayed durable usage state for one selected user session.
#[derive(Clone, Debug, Default)]
pub struct AgentStatisticsReplay {
    pub revision: i64,
    pub agents: HashMap<String, AgentSessionStatisticsSnapshot>,
    pub parents: HashMap<String, String>,
    pub incomplete_legacy_tool_counts: HashSet<String>,
}

/// Failure returned when durable usage-fact append does not complete.
pub type AppendError = Box<dyn Error + Send + Sync>;
type AppendUsageFact = dyn Fn(&Event) -> Result<i64, AppendError> + Send + Sync;

struct RuntimeStatisticsState {
    agents: HashMap<String, Arc<AgentSessionStatistics>>,
    revision: i64,
}

/// Session-scoped live statistics, ordered with completed durable appends and
/// the source-ordered fact/agent/session notifications.
pub struct UserSessionStatistics {
    state: Mutex<RuntimeStatisticsState>,
    append_usage_fact: Box<AppendUsageFact>,
}

impl UserSessionStatistics {
    /// The callback must return only after the fact append has completed.
    pub fn new(
        replay: AgentStatisticsReplay,
        append_usage_fact: impl Fn(&Event) -> Result<i64, AppendError> + Send + Sync + 'static,
    ) -> Self {
        let agents = replay
            .agents
            .into_iter()
            .map(|(id, snapshot)| {
                let snapshot = AgentSessionStatisticsSnapshot {
                    has_incomplete_legacy_tool_counts: contains_incomplete_legacy_tools(
                        &replay.incomplete_legacy_tool_counts,
                        &replay.parents,
                        &id,
                    ),
                    ..snapshot
                };
                (id, Arc::new(AgentSessionStatistics::new(snapshot)))
            })
            .collect();
        Self {
            state: Mutex::new(RuntimeStatisticsState {
                agents,
                revision: replay.revision,
            }),
            append_usage_fact: Box::new(append_usage_fact),
        }
    }

    /// Returns replay-seeded counters when present and inserts an empty counter
    /// exactly once for a new agent ID.
    pub fn get_agent_statistics(&self, session_id: &str) -> Arc<AgentSessionStatistics> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            state
                .agents
                .entry(session_id.to_owned())
                .or_insert_with(|| {
                    Arc::new(AgentSessionStatistics::new(
                        AgentSessionStatisticsSnapshot::default(),
                    ))
                }),
        )
    }

    /// Captures a full cumulative session replacement snapshot at the current
    /// durable revision. No counter is inferred from event publication.
    pub fn capture_usage(&self, root: &AgentSessionStatistics) -> SessionUsageSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        capture_usage(state.revision, &root.capture())
    }

    /// Appends first; stale/duplicate revisions do not apply increments or
    /// publish anything. A new revision applies once and publishes the source
    /// fact, agent self-statistics, then root cumulative usage under one gate.
    pub fn record_usage<P, A>(
        &self,
        publisher: &P,
        fact: Event,
        increment: &AgentUsageIncrement,
        apply: A,
        agent: &AgentSessionStatistics,
        root: &AgentSessionStatistics,
    ) -> Result<bool, AppendError>
    where
        P: Fn(Event),
        A: FnOnce(&AgentUsageIncrement),
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revision = (self.append_usage_fact)(&fact)?;
        if revision <= state.revision {
            return Ok(false);
        }

        apply(increment);
        state.revision = revision;

        publisher(fact.clone());

        let mut agent_update = Event::new();
        agent_update.agent_session_id = fact.agent_session_id.clone();
        let statistics = agent.capture().to_self_statistics();
        agent_update.set_agent_statistics_updated(AgentStatisticsUpdatedEvent {
            input_tokens: statistics.input_tokens,
            cached_input_tokens: statistics.cached_input_tokens,
            output_tokens: statistics.output_tokens,
            context_size: statistics.context_size,
            context_limit: statistics.context_limit,
            input_cost: statistics.input_cost,
            output_cost: statistics.output_cost,
            ..AgentStatisticsUpdatedEvent::new()
        });
        publisher(agent_update);

        let mut usage_snapshot = Event::new();
        usage_snapshot.set_session_usage_snapshot(capture_usage(revision, &root.capture()));
        publisher(usage_snapshot);
        Ok(true)
    }
}

fn contains_incomplete_legacy_tools(
    incomplete: &HashSet<String>,
    parents: &HashMap<String, String>,
    session_id: &str,
) -> bool {
    incomplete.iter().any(|incomplete_id| {
        let mut ancestor = incomplete_id.as_str();
        while !ancestor.is_empty() {
            if ancestor == session_id {
                return true;
            }
            ancestor = parents.get(ancestor).map_or("", String::as_str);
        }
        false
    })
}

fn capture_usage(
    revision: i64,
    statistics: &AgentSessionStatisticsSnapshot,
) -> SessionUsageSnapshot {
    let totals = statistics.cumulative.totals;
    SessionUsageSnapshot {
        revision: u64::try_from(revision).expect("usage revision cannot be negative"),
        input_tokens: totals.input_tokens,
        cached_input_tokens: totals.cached_input_tokens,
        output_tokens: totals.output_tokens,
        input_cost: totals.input_cost,
        output_cost: totals.output_cost,
        context_size: statistics.context_size,
        context_limit: statistics.context_limit,
        ..SessionUsageSnapshot::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::values::accounting::{AgentUsageKey, AgentUsageSnapshot, AgentUsageTotals};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::thread;

    fn snapshot(input_tokens: i64, incomplete: bool) -> AgentSessionStatisticsSnapshot {
        let totals = AgentUsageTotals {
            input_tokens,
            cached_input_tokens: 3,
            output_tokens: 5,
            input_cost: 0.25,
            output_cost: 0.5,
            ..AgentUsageTotals::default()
        };
        let usage = AgentUsageSnapshot {
            totals,
            ..AgentUsageSnapshot::default()
        };
        AgentSessionStatisticsSnapshot {
            own: usage.clone(),
            cumulative: usage,
            context_size: 17,
            context_limit: 64,
            has_incomplete_legacy_tool_counts: incomplete,
        }
    }

    fn increment() -> AgentUsageIncrement {
        AgentUsageIncrement {
            key: AgentUsageKey::legacy(),
            totals: AgentUsageTotals {
                input_tokens: 7,
                cached_input_tokens: 2,
                output_tokens: 4,
                input_cost: 1.25,
                output_cost: 2.5,
                ..AgentUsageTotals::default()
            },
            context_size: Some(21),
            context_limit: Some(128),
        }
    }

    fn fact() -> Event {
        let mut fact = Event::new();
        fact.id = "usage-fact-id".into();
        fact.agent_session_id = "leaf".into();
        fact
    }

    #[test]
    fn replay_marks_incomplete_agents_and_ancestors_and_reuses_live_handles() {
        let replay = AgentStatisticsReplay {
            revision: 8,
            agents: HashMap::from([
                ("root".into(), snapshot(20, true)),
                ("child".into(), snapshot(0, false)),
                ("leaf".into(), snapshot(4, true)),
                ("sibling".into(), snapshot(0, true)),
                ("orphan".into(), snapshot(0, true)),
            ]),
            parents: HashMap::from([
                ("child".into(), "root".into()),
                ("leaf".into(), "child".into()),
                ("sibling".into(), "root".into()),
            ]),
            incomplete_legacy_tool_counts: HashSet::from(["leaf".into()]),
        };
        let state = UserSessionStatistics::new(replay, |_| Ok(9));

        for id in ["root", "child", "leaf"] {
            assert!(
                state
                    .get_agent_statistics(id)
                    .capture()
                    .has_incomplete_legacy_tool_counts
            );
        }
        for id in ["sibling", "orphan"] {
            assert!(
                !state
                    .get_agent_statistics(id)
                    .capture()
                    .has_incomplete_legacy_tool_counts
            );
        }
        assert_eq!(
            state
                .get_agent_statistics("leaf")
                .capture()
                .own
                .totals
                .input_tokens,
            4
        );
        assert!(Arc::ptr_eq(
            &state.get_agent_statistics("leaf"),
            &state.get_agent_statistics("leaf")
        ));
        assert!(!Arc::ptr_eq(
            &state.get_agent_statistics("Agent"),
            &state.get_agent_statistics("agent")
        ));
        assert_eq!(
            state.get_agent_statistics("missing").capture(),
            AgentSessionStatisticsSnapshot::default()
        );
    }

    #[test]
    fn capture_usage_projects_cumulative_values_and_signed_revision() {
        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 0,
                ..Default::default()
            },
            |_| Ok(1),
        );
        let root = AgentSessionStatistics::new(snapshot(20, false));
        let captured = state.capture_usage(&root);
        assert_eq!(captured.revision, 0);
        assert_eq!(captured.input_tokens, 20);
        assert_eq!(captured.cached_input_tokens, 3);
        assert_eq!(captured.output_tokens, 5);
        assert_eq!(captured.input_cost, 0.25);
        assert_eq!(captured.output_cost, 0.5);
        assert_eq!(captured.context_size, 17);
        assert_eq!(captured.context_limit, 64);

        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: -1,
                ..Default::default()
            },
            |_| Ok(1),
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.capture_usage(&root)))
                .is_err()
        );
    }

    #[test]
    fn record_usage_appends_applies_then_publishes_exact_fact_and_snapshots() {
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let append_order = Arc::clone(&order);
        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 10,
                ..Default::default()
            },
            move |fact| {
                assert_eq!(fact.agent_session_id, "leaf");
                append_order.lock().unwrap().push("append");
                Ok(12)
            },
        );
        let leaf = AgentSessionStatistics::new(snapshot(0, false));
        let root = AgentSessionStatistics::new(snapshot(100, false));
        let add_order = Arc::clone(&order);
        let publish_order = Arc::clone(&order);
        let sent = Mutex::new(Vec::new());
        let increment = increment();
        assert!(
            state
                .record_usage(
                    &|event| {
                        publish_order.lock().unwrap().push(
                            if event.has_agent_statistics_updated() {
                                "agent"
                            } else if event.has_session_usage_snapshot() {
                                "session"
                            } else {
                                "fact"
                            },
                        );
                        sent.lock().unwrap().push(event);
                    },
                    fact(),
                    &increment,
                    |increment| {
                        add_order.lock().unwrap().push("apply");
                        leaf.add_own(increment).unwrap();
                        root.add_descendant(increment).unwrap();
                    },
                    &leaf,
                    &root,
                )
                .unwrap()
        );
        assert_eq!(
            *order.lock().unwrap(),
            ["append", "apply", "fact", "agent", "session"]
        );
        let sent = sent.into_inner().unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].id, "usage-fact-id");
        assert_eq!(sent[0].agent_session_id, "leaf");
        let agent_update = sent[1].agent_statistics_updated();
        assert_eq!(agent_update.input_tokens, 7);
        assert_eq!(agent_update.cached_input_tokens, 5);
        assert_eq!(agent_update.output_tokens, 9);
        assert_eq!(agent_update.context_size, 21);
        assert_eq!(agent_update.context_limit, 128);
        assert_eq!(agent_update.input_cost, 1.5);
        assert_eq!(agent_update.output_cost, 3.0);
        let usage = sent[2].session_usage_snapshot();
        assert_eq!(usage.revision, 12);
        assert_eq!(usage.input_tokens, 107);
        assert_eq!(usage.cached_input_tokens, 5);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.input_cost, 1.5);
        assert_eq!(usage.output_cost, 3.0);
        assert_eq!(usage.context_size, 17);
        assert_eq!(usage.context_limit, 64);
    }

    #[test]
    fn stale_or_duplicate_revisions_append_but_do_not_apply_or_publish() {
        let values = Arc::new([12_i64, 12, 9, 13]);
        let position = Arc::new(AtomicUsize::new(0));
        let append_values = Arc::clone(&values);
        let append_position = Arc::clone(&position);
        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 10,
                ..Default::default()
            },
            move |_| {
                let index = append_position.fetch_add(1, Ordering::SeqCst);
                Ok(append_values[index])
            },
        );
        let leaf = AgentSessionStatistics::default();
        let root = AgentSessionStatistics::default();
        let apply_count = AtomicUsize::new(0);
        let publish_count = AtomicUsize::new(0);
        for (index, expected) in [true, false, false, true].into_iter().enumerate() {
            let applied = &apply_count;
            let published = &publish_count;
            let actual = state
                .record_usage(
                    &|_| {
                        published.fetch_add(1, Ordering::SeqCst);
                    },
                    fact(),
                    &increment(),
                    |_| {
                        applied.fetch_add(1, Ordering::SeqCst);
                    },
                    &leaf,
                    &root,
                )
                .unwrap();
            assert_eq!(actual, expected, "revision result at append {index}");
        }
        assert_eq!(apply_count.load(Ordering::SeqCst), 2);
        assert_eq!(publish_count.load(Ordering::SeqCst), 6);
        assert_eq!(state.capture_usage(&root).revision, 13);
    }

    #[test]
    fn concurrent_records_serialize_complete_append_apply_publish_sequences() {
        let trace = Arc::new(Mutex::new(Vec::<String>::new()));
        let append_trace = Arc::clone(&trace);
        let revision = Arc::new(AtomicI64::new(10));
        let append_revision = Arc::clone(&revision);
        let state = Arc::new(UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 10,
                ..Default::default()
            },
            move |fact| {
                append_trace
                    .lock()
                    .unwrap()
                    .push(format!("append:{}", fact.id));
                Ok(append_revision.fetch_add(1, Ordering::SeqCst) + 1)
            },
        ));
        let root = Arc::new(AgentSessionStatistics::default());
        let first_agent = Arc::new(AgentSessionStatistics::default());
        let second_agent = Arc::new(AgentSessionStatistics::default());
        let start = Arc::new(Barrier::new(3));

        thread::scope(|scope| {
            for (id, amount, agent) in [
                ("a", 1, Arc::clone(&first_agent)),
                ("b", 2, Arc::clone(&second_agent)),
            ] {
                let state = Arc::clone(&state);
                let root = Arc::clone(&root);
                let trace = Arc::clone(&trace);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    let mut fact = Event::new();
                    fact.id = format!("fact-{id}");
                    fact.agent_session_id = format!("agent-{id}");
                    let increment = AgentUsageIncrement {
                        key: AgentUsageKey::legacy(),
                        totals: AgentUsageTotals {
                            input_tokens: amount,
                            ..AgentUsageTotals::default()
                        },
                        context_size: None,
                        context_limit: None,
                    };
                    start.wait();
                    state
                        .record_usage(
                            &|event| {
                                let entry = if event.has_agent_statistics_updated() {
                                    format!("agent:{}", event.agent_session_id)
                                } else if event.has_session_usage_snapshot() {
                                    "session".to_owned()
                                } else {
                                    format!("fact:{}", event.id)
                                };
                                trace.lock().unwrap().push(entry);
                            },
                            fact,
                            &increment,
                            |increment| {
                                trace
                                    .lock()
                                    .unwrap()
                                    .push(format!("apply:{}", increment.totals.input_tokens));
                                agent.add_own(increment).unwrap();
                                root.add_descendant(increment).unwrap();
                            },
                            &agent,
                            &root,
                        )
                        .unwrap();
                });
            }
            start.wait();
        });

        let trace = trace.lock().unwrap().clone();
        assert_eq!(trace.len(), 10);
        for transaction in trace.chunks_exact(5) {
            assert!(transaction[0].starts_with("append:fact-"));
            assert!(transaction[1].starts_with("apply:"));
            assert_eq!(
                transaction[2],
                transaction[0].replacen("append:", "fact:", 1)
            );
            let id = transaction[0].strip_prefix("append:fact-").unwrap();
            assert_eq!(transaction[3], format!("agent:agent-{id}"));
            assert_eq!(transaction[4], "session");
        }
        assert_eq!(root.capture().cumulative.totals.input_tokens, 3);
        assert_eq!(first_agent.capture().own.totals.input_tokens, 1);
        assert_eq!(second_agent.capture().own.totals.input_tokens, 2);
        assert_eq!(state.capture_usage(&root).revision, 12);
    }

    #[test]
    fn append_failure_has_no_apply_revision_or_publication() {
        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 10,
                ..Default::default()
            },
            |_| Err(std::io::Error::other("private append failure").into()),
        );
        let root = AgentSessionStatistics::default();
        let apply_count = AtomicUsize::new(0);
        let publish_count = AtomicUsize::new(0);
        assert!(
            state
                .record_usage(
                    &|_| {
                        publish_count.fetch_add(1, Ordering::SeqCst);
                    },
                    fact(),
                    &increment(),
                    |_| {
                        apply_count.fetch_add(1, Ordering::SeqCst);
                    },
                    &root,
                    &root,
                )
                .is_err()
        );
        assert_eq!(apply_count.load(Ordering::SeqCst), 0);
        assert_eq!(publish_count.load(Ordering::SeqCst), 0);
        assert_eq!(state.capture_usage(&root).revision, 10);
    }

    #[test]
    fn apply_and_each_publication_panic_keep_source_order_failure_boundaries() {
        let state = UserSessionStatistics::new(
            AgentStatisticsReplay {
                revision: 10,
                ..Default::default()
            },
            |_| Ok(11),
        );
        let root = AgentSessionStatistics::default();
        let partial_increment = increment();
        let apply_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.record_usage(
                &|_| panic!("publisher must not run"),
                fact(),
                &partial_increment,
                |increment| {
                    root.add_own(increment).unwrap();
                    panic!("apply failed after partial mutation");
                },
                &root,
                &root,
            );
        }));
        assert!(apply_result.is_err());
        assert_eq!(state.capture_usage(&root).revision, 10);
        assert_eq!(root.capture().own.totals.input_tokens, 7);

        for failed_publication in 0..3 {
            let state = UserSessionStatistics::new(
                AgentStatisticsReplay {
                    revision: 10,
                    ..Default::default()
                },
                |_| Ok(11),
            );
            let root = AgentSessionStatistics::default();
            let attempts = AtomicUsize::new(0);
            let applies = AtomicUsize::new(0);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = state.record_usage(
                        &|_| {
                            if attempts.fetch_add(1, Ordering::SeqCst) == failed_publication {
                                panic!("publication failed");
                            }
                        },
                        fact(),
                        &increment(),
                        |_| {
                            applies.fetch_add(1, Ordering::SeqCst);
                        },
                        &root,
                        &root,
                    );
                }))
                .is_err()
            );
            assert_eq!(attempts.load(Ordering::SeqCst), failed_publication + 1);
            assert_eq!(state.capture_usage(&root).revision, 11);
            assert!(
                !state
                    .record_usage(
                        &|_| panic!("duplicate revision published"),
                        fact(),
                        &increment(),
                        |_| panic!("duplicate applied"),
                        &root,
                        &root,
                    )
                    .unwrap()
            );
            assert_eq!(applies.load(Ordering::SeqCst), 1);
        }
    }
}
