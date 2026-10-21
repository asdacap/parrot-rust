use crate::{fixtures, support::Context};
use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::{
    Configuration, Result,
    configuration::ModelConfigurationTransaction,
    models::{ModelPresetConfig, ModelPresetContextLimits, ProviderModelAliasDefaults},
};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

fn bounded(operation: impl FnOnce() + Send + 'static) {
    let (done, completed) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        operation();
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(10))
        .expect("transaction did not complete (possible admission deadlock)");
    worker.join().unwrap();
}

fn limits() -> ModelPresetContextLimits {
    ModelPresetContextLimits {
        default: Some(ContextSize::parse("12k").unwrap()),
        model_aliases: BTreeMap::from([("low_llm".into(), ContextSize::parse("50%").unwrap())]),
    }
}

fn defaults() -> ProviderModelAliasDefaults {
    ProviderModelAliasDefaults {
        provider_id: "p".into(),
        low_model_string: "p/low".into(),
        medium_model_string: "p/medium".into(),
        high_model_string: "p/high".into(),
        xhigh_model_string: "p/xhigh".into(),
    }
}

fn preset() -> ModelPresetConfig {
    ModelPresetConfig {
        model: "p/preset".into(),
        model_aliases: BTreeMap::from([("low_llm".into(), "p/saved".into())]),
        context_limits: Some(limits()),
    }
}

fn token_update(transaction: &mut ModelConfigurationTransaction<'_>, kind: usize) -> Result<()> {
    match kind {
        0 => transaction.set_model_alias("low_llm", "p/alias"),
        1 => transaction.set_model_aliases(&defaults()),
        2 => transaction.set_model_preset("Work", &preset()),
        3 => transaction.set_model_routing_with_limits(
            "p/routed",
            &BTreeMap::from([("low_llm".into(), "p/routed-alias".into())]),
            Some(&limits()),
        ),
        4 => transaction.set_context_limit(ContextSize::parse("24k").unwrap()),
        _ => unreachable!(),
    }
}

fn direct_update(configuration: &mut Configuration, kind: usize) -> Result<()> {
    match kind {
        0 => configuration.set_model_alias("low_llm", "p/alias"),
        1 => configuration.set_model_aliases(&defaults()),
        2 => configuration.set_model_preset("Work", &preset()),
        3 => configuration.set_model_routing_with_limits(
            "p/routed",
            &BTreeMap::from([("low_llm".into(), "p/routed-alias".into())]),
            Some(&limits()),
        ),
        4 => configuration.set_context_limit(ContextSize::parse("24k").unwrap()),
        5 => configuration.set_model("p/direct"),
        6 => configuration.set_skill_enabled("/workspace/skills/example", true),
        _ => unreachable!(),
    }
}

#[test]
fn transaction_mutations_match_direct_setters_entire_yaml_and_snapshots() {
    let direct_context = Context::new();
    let transaction_context = Context::new();
    let initial = "model: p/original\nunknown:\n  keep: true\n";
    let mut direct = direct_context.configured(initial).unwrap();
    let mut transactional = transaction_context.configured(initial).unwrap();
    for kind in 0..5 {
        direct_update(&mut direct, kind).unwrap();
        let snapshot = transactional
            .model_configuration_transaction(|transaction| {
                token_update(transaction, kind).unwrap();
                transaction.snapshot()
            })
            .unwrap();
        assert_eq!(snapshot, direct.model_configuration());
        assert_eq!(transaction_context.saved(), direct_context.saved());
    }
    let before = transaction_context.saved();
    let missing = transactional
        .model_configuration_transaction(|transaction| {
            transaction.set_model_alias("missing", "p/new")
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(
        missing,
        direct.set_model_alias("missing", "p/new").unwrap_err()
    );
    let invalid = transactional
        .model_configuration_transaction(|transaction| {
            transaction.set_model_routing_with_limits("not a selector", &BTreeMap::new(), None)
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(
        invalid,
        direct
            .set_model_routing_with_limits("not a selector", &BTreeMap::new(), None)
            .unwrap_err()
    );
    assert_eq!(transaction_context.saved(), before);
}

#[test]
fn transaction_reenters_separate_normalized_handles_and_excludes_every_direct_writer() {
    bounded(|| {
        let context = Context::new();
        let mut outer = context.configured("unknown: retained\n").unwrap();
        let mut nested = fixtures::fixture_load(
            &context.directory.join(".").join("config.yaml"),
            &context.predefined,
        )
        .unwrap();
        let competitors: Vec<_> = (0..7).map(|_| context.load().unwrap()).collect();
        let independent_context = Context::new();
        let mut independent = independent_context.configured("unknown: other\n").unwrap();
        let (ready, started) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let mut workers = Vec::new();
        outer
            .model_configuration_transaction(|transaction| {
                for (kind, mut configuration) in competitors.into_iter().enumerate() {
                    let ready = ready.clone();
                    let done = done.clone();
                    workers.push(std::thread::spawn(move || {
                        ready.send(()).unwrap();
                        direct_update(&mut configuration, kind).unwrap();
                        done.send(kind).unwrap();
                    }));
                }
                for _ in 0..7 {
                    started.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                assert!(completed.recv_timeout(Duration::from_millis(100)).is_err());
                transaction.set_model_alias("low_llm", "p/staged").unwrap();
                // These direct and nested calls must re-enter on the SAME thread.
                nested.set_model("p/callback").unwrap();
                nested
                    .model_configuration_transaction(|inner| {
                        let refreshed = inner.reload().unwrap();
                        inner.synchronize(&refreshed);
                        inner.set_context_limit(ContextSize::parse("8k").unwrap())
                    })
                    .unwrap()
                    .unwrap();
                assert!(completed.recv_timeout(Duration::from_millis(100)).is_err());
                let (other_done, other_completed) = mpsc::channel();
                let other = std::thread::spawn(move || {
                    independent.set_model("p/other").unwrap();
                    other_done.send(()).unwrap();
                });
                other_completed
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                other.join().unwrap();
            })
            .unwrap();
        let mut completed_kinds = Vec::new();
        for _ in 0..7 {
            completed_kinds.push(completed.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        completed_kinds.sort_unstable();
        assert_eq!(completed_kinds, (0..7).collect::<Vec<_>>());
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(context.saved().contains("unknown: retained"));
    });
}

#[test]
fn transaction_reload_synchronize_and_explicit_rollback_keep_caller_error_inside_gate() {
    bounded(|| {
        let context = Context::new();
        let mut configuration = context
            .configured("model: p/old\nsandbox:\n  enabled: false\nunknown: retained\n")
            .unwrap();
        context.write("model: p/external\nsandbox:\n  enabled: true\nunknown: retained\n");
        let previous = configuration.reload_model_configuration().unwrap();
        let mut competitor = context.load().unwrap();
        let (ready, started) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let mut worker = None;
        let result: std::result::Result<(), &str> = configuration
            .model_configuration_transaction(|transaction| {
                let refreshed = transaction.reload().unwrap();
                assert_eq!(refreshed.model, "p/external");
                assert!(refreshed.sandbox_enabled);
                transaction.synchronize(&refreshed);
                let before = transaction.snapshot();
                assert_eq!(before.model, "p/external");
                assert!(!before.sandbox_enabled);
                transaction
                    .set_model_routing_with_limits("p/new", &BTreeMap::new(), None)
                    .unwrap();
                worker = Some(std::thread::spawn(move || {
                    ready.send(()).unwrap();
                    competitor.set_model_alias("low_llm", "p/after").unwrap();
                    done.send(()).unwrap();
                }));
                started.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(completed.recv_timeout(Duration::from_millis(100)).is_err());
                let callback_result: std::result::Result<(), &str> = Err("rejected");
                if callback_result.is_err() {
                    // Source-specific rollback is explicit caller work, not automatic.
                    let targets = previous
                        .model_aliases
                        .iter()
                        .map(|(name, alias)| (name.clone(), alias.model_string.clone()))
                        .collect();
                    let limits = ModelPresetContextLimits {
                        default: previous.context_limit.clone(),
                        model_aliases: previous
                            .model_aliases
                            .iter()
                            .filter_map(|(name, alias)| {
                                alias
                                    .context_limit
                                    .clone()
                                    .map(|limit| (name.clone(), limit))
                            })
                            .collect(),
                    };
                    transaction
                        .set_model_routing_with_limits(&previous.model, &targets, Some(&limits))
                        .unwrap();
                    transaction.synchronize(&previous);
                }
                assert!(completed.recv_timeout(Duration::from_millis(100)).is_err());
                callback_result
            })
            .unwrap();
        assert_eq!(result, Err("rejected"));
        completed.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.unwrap().join().unwrap();
        assert_eq!(configuration.model, "p/external");
        assert!(!configuration.runtime.sandbox_enabled);
        let final_snapshot = configuration.reload_model_configuration().unwrap();
        assert_eq!(final_snapshot.model, "p/external");
        assert_eq!(
            final_snapshot.model_aliases["low_llm"].model_string,
            "p/after"
        );
        assert!(context.saved().contains("unknown: retained"));
    });
}

#[test]
fn transaction_errors_do_not_implicitly_rollback_and_panics_release_reentrant_admission() {
    bounded(|| {
        let context = Context::new();
        let mut configuration = context.configured("model: p/old\n").unwrap();
        let mut nested = context.load().unwrap();
        let result: std::result::Result<(), &str> = configuration
            .model_configuration_transaction(|transaction| {
                transaction
                    .set_model_alias("low_llm", "p/persisted")
                    .unwrap();
                Err("caller failure")
            })
            .unwrap();
        assert_eq!(result, Err("caller failure"));
        assert_eq!(
            configuration
                .reload_model_configuration()
                .unwrap()
                .model_aliases["low_llm"]
                .model_string,
            "p/persisted"
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            configuration
                .model_configuration_transaction(|_| {
                    nested
                        .model_configuration_transaction(|_| panic!("callback panic"))
                        .unwrap();
                })
                .unwrap();
        }));
        assert!(panic.is_err());
        let mut subsequent = context.load().unwrap();
        let (done, completed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            subsequent.set_model("p/after-panic").unwrap();
            done.send(()).unwrap();
        });
        completed.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        configuration
            .model_configuration_transaction(|transaction| {
                let refreshed = transaction.reload().unwrap();
                transaction.synchronize(&refreshed);
                assert_eq!(transaction.snapshot().model, "p/after-panic");
            })
            .unwrap();
    });
}
