use parrot_domain::values::accounting::*;
use std::sync::Arc;
use std::thread;

fn key(effort: Option<&str>) -> AgentUsageKey {
    AgentUsageKey::from_request("provider", "model", effort.map(str::to_owned))
}

fn increment(key: AgentUsageKey, totals: AgentUsageTotals) -> AgentUsageIncrement {
    AgentUsageIncrement {
        key,
        totals,
        context_size: Some(100),
        context_limit: Some(1000),
    }
}

#[test]
fn statistics_preserve_self_subtree_buckets_context_and_replay() {
    for repetitions in [1, 100] {
        let root = AgentSessionStatistics::default();
        let child = AgentSessionStatistics::default();
        let grandchild = AgentSessionStatistics::default();
        let own = increment(
            key(Some("high")),
            AgentUsageTotals {
                input_tokens: 100,
                cached_input_tokens: 25,
                output_tokens: 10,
                input_cost: 0.5,
                output_cost: 0.25,
                ..AgentUsageTotals::default()
            },
        );
        root.add_own(&own).unwrap();
        let original = root.capture();
        let descendant = AgentUsageIncrement {
            context_size: Some(20),
            context_limit: Some(200),
            ..increment(
                key(Some("low")),
                AgentUsageTotals {
                    input_tokens: 20,
                    cached_input_tokens: 5,
                    output_tokens: 2,
                    input_cost: 0.125,
                    output_cost: 0.0625,
                    ..AgentUsageTotals::default()
                },
            )
        };
        for _ in 0..repetitions {
            let tool = AgentUsageIncrement::from_tool_start(key(Some("low")));
            for usage in [&descendant, &tool] {
                grandchild.add_own(usage).unwrap();
                child.add_descendant(usage).unwrap();
                root.add_descendant(usage).unwrap();
            }
        }
        root.add_descendant(&increment(
            key(None),
            AgentUsageTotals {
                input_tokens: 1,
                output_tokens: 1,
                tool_calls: 1,
                ..AgentUsageTotals::default()
            },
        ))
        .unwrap();
        root.add_descendant(&AgentUsageIncrement {
            key: AgentUsageKey::legacy(),
            context_size: None,
            context_limit: None,
            totals: AgentUsageTotals {
                input_tokens: 3,
                cached_input_tokens: 1,
                output_tokens: 1,
                ..AgentUsageTotals::default()
            },
        })
        .unwrap();
        let snapshot = root.capture();
        assert_eq!(snapshot.own.totals, own.totals);
        assert_eq!(
            snapshot.cumulative.totals.input_tokens,
            104 + 20 * repetitions
        );
        assert_eq!(
            snapshot.cumulative.totals.cached_input_tokens,
            26 + 5 * repetitions
        );
        assert_eq!(
            snapshot.cumulative.totals.output_tokens,
            12 + 2 * repetitions
        );
        assert_eq!(snapshot.cumulative.totals.tool_calls, repetitions + 1);
        assert_eq!(
            snapshot.cumulative.totals.total_cost(),
            0.75 + 0.1875 * repetitions as f64
        );
        assert_eq!(snapshot.cumulative.models.len(), 4);
        assert_eq!((snapshot.context_size, snapshot.context_limit), (100, 1000));
        assert_eq!(snapshot.to_self_statistics().input_cost, 0.5);
        assert_eq!(child.capture().own.totals, AgentUsageTotals::default());
        assert_eq!(
            child.capture().cumulative.totals,
            grandchild.capture().own.totals
        );
        assert_eq!(original.cumulative.totals, own.totals);
        let restored = AgentSessionStatistics::new(snapshot.clone());
        restored
            .add_own(&AgentUsageIncrement::from_tool_start(key(Some("high"))))
            .unwrap();
        assert_eq!(restored.capture().own.totals.tool_calls, 1);
        assert_eq!(
            restored.capture().cumulative.totals.tool_calls,
            repetitions + 2
        );
        assert_eq!(snapshot.own.totals.tool_calls, 0);
    }
}

#[test]
fn completion_uses_actual_identity_and_cached_pricing_presence() {
    for cached_price in [None, Some(0.0625), Some(0.0)] {
        let key = AgentUsageKey::from_request("provider", "canonical-model", Some("high".into()));
        let usage = AgentUsageIncrement::from_completion(
            key.clone(),
            AgentCompletionUsage {
                input_tokens: 100,
                cached_input_tokens: 20,
                output_tokens: 10,
            },
            AgentUsagePricing {
                input_price: 0.125,
                cached_input_price: cached_price,
                output_price: 0.25,
                context_window: 1000,
            },
        );
        assert_eq!(usage.key, key);
        assert_eq!(usage.totals.input_tokens, 100);
        assert_eq!(usage.totals.cached_input_tokens, 20);
        assert_eq!(usage.totals.output_tokens, 10);
        assert_eq!(
            usage.totals.input_cost,
            10.0 + 20.0 * cached_price.unwrap_or(0.125)
        );
        assert_eq!(usage.totals.output_cost, 2.5);
        assert_eq!(usage.context_size, Some(100));
        assert_eq!(usage.context_limit, Some(1000));
    }
}

#[test]
fn pricing_clamps_only_priced_input_not_recorded_usage() {
    for (input, cached, output, input_cost) in
        [(-5, 10, -2, 0.0), (10, -5, 2, 20.0), (10, 20, 2, 10.0)]
    {
        let usage = AgentUsageIncrement::from_completion(
            key(None),
            AgentCompletionUsage {
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
            },
            AgentUsagePricing {
                input_price: 2.0,
                cached_input_price: Some(1.0),
                output_price: 3.0,
                context_window: 100,
            },
        );
        assert_eq!(usage.totals.input_tokens, input);
        assert_eq!(usage.totals.cached_input_tokens, cached);
        assert_eq!(usage.totals.output_tokens, output);
        assert_eq!(usage.totals.input_cost, input_cost);
        assert_eq!(usage.totals.output_cost, output as f64 * 3.0);
        assert_eq!(usage.context_size, Some(input));
    }
}

#[test]
fn own_context_updates_independently_and_legacy_flag_survives() {
    let statistics = AgentSessionStatistics::new(AgentSessionStatisticsSnapshot {
        context_size: 100,
        context_limit: 1000,
        has_incomplete_legacy_tool_counts: true,
        ..AgentSessionStatisticsSnapshot::default()
    });
    let mut usage = AgentUsageIncrement::from_tool_start(key(None));
    usage.context_size = Some(0);
    statistics.add_own(&usage).unwrap();
    assert_eq!(
        (
            statistics.capture().context_size,
            statistics.capture().context_limit
        ),
        (0, 1000)
    );
    usage.context_size = None;
    usage.context_limit = Some(2000);
    statistics.add_own(&usage).unwrap();
    statistics
        .add_descendant(&increment(key(None), AgentUsageTotals::default()))
        .unwrap();
    let snapshot = statistics.capture();
    assert_eq!((snapshot.context_size, snapshot.context_limit), (0, 2000));
    assert!(snapshot.has_incomplete_legacy_tool_counts);
}

#[test]
fn all_integer_totals_are_checked_but_costs_use_float_sums() {
    for index in 0..4 {
        for (initial, delta) in [(i64::MAX, 1), (i64::MIN, -1)] {
            let mut totals = AgentUsageTotals::default();
            let mut change = AgentUsageTotals::default();
            match index {
                0 => {
                    totals.input_tokens = initial;
                    change.input_tokens = delta;
                }
                1 => {
                    totals.cached_input_tokens = initial;
                    change.cached_input_tokens = delta;
                }
                2 => {
                    totals.output_tokens = initial;
                    change.output_tokens = delta;
                }
                _ => {
                    totals.tool_calls = initial;
                    change.tool_calls = delta;
                }
            }
            assert_eq!(totals.add(&change), Err(UsageOverflow));
        }
    }
    let totals = AgentUsageTotals {
        input_cost: f64::MAX,
        output_cost: f64::MAX,
        ..AgentUsageTotals::default()
    };
    let sum = totals.add(&totals).unwrap();
    assert!(sum.input_cost.is_infinite());
    assert!(sum.output_cost.is_infinite());
    assert!(totals.total_cost().is_infinite());
}

#[test]
fn overflow_leaves_entire_statistics_unchanged() {
    let usage = AgentUsageIncrement::from_tool_start(key(None));
    let mut snapshot = AgentSessionStatisticsSnapshot::default();
    snapshot.cumulative.totals.tool_calls = i64::MAX;
    let statistics = AgentSessionStatistics::new(snapshot.clone());
    assert_eq!(statistics.add_own(&usage), Err(UsageOverflow));
    assert_eq!(statistics.capture(), snapshot);
    assert_eq!(statistics.add_descendant(&usage), Err(UsageOverflow));
    assert_eq!(statistics.capture(), snapshot);
    let mut bucket = AgentUsageSnapshot::default();
    bucket.models.insert(
        usage.key.clone(),
        AgentUsageTotals {
            tool_calls: i64::MAX,
            ..AgentUsageTotals::default()
        },
    );
    assert_eq!(bucket.add(&usage), Err(UsageOverflow));
    assert_eq!(bucket.totals.tool_calls, 0);
}

#[test]
fn provider_model_and_effort_all_separate_buckets() {
    let mut snapshot = AgentUsageSnapshot::default();
    for (provider, model, effort) in [
        ("p", "m", None),
        ("P", "m", None),
        ("p", "M", None),
        ("p", "m", Some("high")),
        ("p", "m", Some("High")),
        ("p", "m", Some("")),
    ] {
        snapshot = snapshot
            .add(&AgentUsageIncrement::from_tool_start(
                AgentUsageKey::from_request(provider, model, effort.map(str::to_owned)),
            ))
            .unwrap();
    }
    assert_eq!(snapshot.models.len(), 6);
    assert_eq!(snapshot.totals.tool_calls, 6);
}

#[test]
fn concurrent_sibling_and_own_updates_do_not_lose_usage() {
    let statistics = Arc::new(AgentSessionStatistics::default());
    thread::scope(|scope| {
        for worker in 0..10 {
            let statistics = Arc::clone(&statistics);
            scope.spawn(move || {
                let usage = AgentUsageIncrement::from_tool_start(key(None));
                for _ in 0..100 {
                    if worker % 2 == 0 {
                        statistics.add_own(&usage).unwrap();
                    } else {
                        statistics.add_descendant(&usage).unwrap();
                    }
                }
            });
        }
    });
    let snapshot = statistics.capture();
    assert_eq!(snapshot.own.totals.tool_calls, 500);
    assert_eq!(snapshot.cumulative.totals.tool_calls, 1000);
    assert_eq!(snapshot.cumulative.models[&key(None)].tool_calls, 1000);
}

#[test]
fn pending_and_retained_reservations_have_exact_once_transitions() {
    let budget = RetainedAgentBudget::new(2);
    let pending = budget.reserve().unwrap();
    let retained = budget.reserve().unwrap();
    retained.commit();
    retained.commit();
    assert_eq!(budget.reserve().unwrap_err(), RetainedAgentLimitReached);
    pending.rollback();
    pending.rollback();
    pending.commit();
    pending.release();
    let replacement = budget.reserve().unwrap();
    replacement.commit();
    replacement.rollback();
    assert_eq!(budget.reserve().unwrap_err(), RetainedAgentLimitReached);
    retained.release();
    retained.release();
    retained.commit();
    let final_reservation = budget.reserve().unwrap();
    final_reservation.release();
    assert!(budget.reserve().is_err());
    final_reservation.rollback();
    replacement.release();
    let first = budget.reserve().unwrap();
    let second = budget.reserve().unwrap();
    assert!(budget.reserve().is_err());
    first.rollback();
    second.rollback();
}

#[test]
fn zero_and_negative_limits_reject_and_maximum_capacity_is_accepted() {
    for capacity in [0, -1, i32::MIN] {
        let error = RetainedAgentBudget::new(capacity).reserve().unwrap_err();
        assert_eq!(error.to_string(), "subagent retention limit reached");
    }
    let budget = RetainedAgentBudget::new(i32::MAX);
    let reservation = budget.reserve().unwrap();
    reservation.commit();
    reservation.release();
    budget.reserve().unwrap().rollback();
}

#[test]
fn concurrent_reservations_and_duplicate_transitions_preserve_capacity() {
    let budget = RetainedAgentBudget::new(1);
    let reservation = budget.reserve().unwrap();
    thread::scope(|scope| {
        for _ in 0..20 {
            let reservation = reservation.clone();
            scope.spawn(move || {
                reservation.commit();
                reservation.rollback();
                reservation.release();
            });
        }
    });
    let next = budget.reserve().unwrap();
    thread::scope(|scope| {
        for _ in 0..20 {
            let budget = budget.clone();
            scope.spawn(move || assert!(budget.reserve().is_err()));
        }
    });
    next.rollback();
    let wins = thread::scope(|scope| {
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let budget = budget.clone();
                scope.spawn(move || budget.reserve())
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .collect::<Vec<_>>()
    });
    assert_eq!(wins.len(), 1);
    wins[0].rollback();
}
