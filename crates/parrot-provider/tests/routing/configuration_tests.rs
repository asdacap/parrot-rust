use super::alias_tests::{registry, routing};
use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::{ConfigErrorKind, Configuration};
use parrot_provider::{
    routing::{aliases::*, configuration::*},
    sessions::Error,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    predefined: PathBuf,
}
impl Fixture {
    fn new(content: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::current_dir().unwrap().join(".tmp");
        std::fs::create_dir_all(&base).unwrap();
        let directory = loop {
            let candidate = base.join(format!(
                "t32-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create config fixture: {error}"),
            }
        };
        let fixture = Self {
            path: directory.join("config.yaml"),
            predefined: directory.join("predefined.yaml"),
            directory,
        };
        fixture.write(content);
        fixture
    }
    fn write(&self, content: &str) {
        std::fs::write(&self.path, content).unwrap();
    }
    fn load(&self) -> Configuration {
        let environment = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "WORKDIR",
        ]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                self.directory.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
        Configuration::load(&self.path, &self.predefined, &environment, |_| Ok(())).unwrap()
    }
    fn coordinator(
        &self,
    ) -> (
        Arc<ModelConfigurationCoordinator>,
        Arc<ModelRouter>,
        Arc<ModelRouting>,
    ) {
        let configuration = self.load();
        let (routing, router) = routing(
            registry(&["old", "new", "later", "model"]),
            &configuration.model,
            definitions_from_config(&configuration.models.model_aliases).unwrap(),
        );
        (
            Arc::new(ModelConfigurationCoordinator::new(
                configuration,
                routing.clone(),
                router.clone(),
            )),
            router,
            routing,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}
const INITIAL: &str =
    "model: preferred\nmodel_aliases:\n  preferred:\n    model_string: p/old\n    usage: Primary\n";

#[test]
fn model_configuration_refresh_publishes_one_valid_generation_and_rejects_invalid_edits() {
    let fixture = Fixture::new(INITIAL);
    let (coordinator, router, _) = fixture.coordinator();
    let initial = router.resolve("").unwrap();
    fixture.write("model: preferred\nmodel_aliases:\n  preferred:\n    model_string: p/new\n    usage: Updated primary\n");
    let refreshed = coordinator.refresh().unwrap();
    let after = router.resolve("").unwrap();
    fixture.write("model_aliases: []\n");
    let refused = coordinator.refresh().err().unwrap();
    let after_refusal = router.resolve("").unwrap();
    assert_eq!(initial.canonical_model.selector(), "p/old");
    assert_eq!(after.canonical_model.selector(), "p/new");
    assert_eq!(after.alias.unwrap().usage, "Updated primary");
    assert!(refreshed.revision > initial.routing_snapshot.revision);
    let CoordinatorError::Foundation(error) = &refused else {
        panic!("expected Foundation validation error: {refused}");
    };
    assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
    assert_eq!(error.to_string(), "model_aliases must be a mapping");
    assert_eq!(refused.to_string(), error.to_string());
    assert_eq!(after_refusal.canonical_model.selector(), "p/new");
    assert_eq!(after_refusal.routing_snapshot.revision, refreshed.revision);
}

#[test]
fn model_preset_snapshot_overwrites_and_selection_overlays_later_aliases() {
    let fixture = Fixture::new(
        "model: preferred\nmodel_aliases:\n  preferred:\n    model_string: p/old\n    usage: Primary\n  empty:\n    model_string: \"\"\n    usage: Disabled\n",
    );
    let (coordinator, _, _) = fixture.coordinator();
    let first = coordinator.set_preset("Work", "preferred").unwrap();
    let overwritten = coordinator.set_preset("Work", "p/new").unwrap();
    coordinator.set_preset("Work", "preferred").unwrap();
    fixture.write("model: p/new\nmodel_presets:\n  Work:\n    model: preferred\n    model_aliases:\n      empty: \"\"\n      high_llm: \"\"\n      low_llm: \"\"\n      medium_llm: \"\"\n      preferred: p/old\n      xhigh_llm: \"\"\nmodel_aliases:\n  preferred:\n    model_string: p/new\n    usage: Changed\n  empty:\n    model_string: p/later\n    usage: Enabled later\n  later:\n    model_string: p/later\n    usage: Added later\n");
    let selected = coordinator
        .select_preset("Work", "preferred", |_| Ok(()))
        .unwrap()
        .resolved_selection
        .routing_snapshot;
    let reloaded = fixture.load();
    assert_eq!(first.model_aliases["empty"], "");
    assert!(first.model_aliases.contains_key("preferred"));
    assert_eq!(overwritten.model, "p/new");
    assert_eq!(selected.configured_default_selector, "preferred");
    assert_eq!(
        selected.aliases.find("preferred").unwrap().model_string,
        "p/old"
    );
    assert_eq!(selected.aliases.find("empty").unwrap().model_string, "");
    assert_eq!(
        selected.aliases.find("later").unwrap().model_string,
        "p/later"
    );
    assert_eq!(reloaded.model, "preferred");
    assert_eq!(
        reloaded.models.model_aliases["later"].model_string,
        "p/later"
    );
    assert_eq!(reloaded.models.model_presets["Work"].model, "preferred");
}

#[test]
fn concurrent_preset_and_alias_mutations_publish_only_complete_generations() {
    let fixture = Fixture::new(INITIAL);
    let (coordinator, router, routing) = fixture.coordinator();
    coordinator.set_preset("Work", "preferred").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let select = coordinator.clone();
        let start = barrier.clone();
        let selected = scope.spawn(move || {
            start.wait();
            select
                .select_preset("Work", "preferred", |_| Ok(()))
                .unwrap()
        });
        let configure = coordinator.clone();
        let start = barrier.clone();
        let configured = scope.spawn(move || {
            start.wait();
            configure.configure_alias("preferred", "p/new").unwrap()
        });
        barrier.wait();
        selected.join().unwrap();
        configured.join().unwrap();
    });
    let persisted = fixture.load();
    let live = routing.capture();
    assert_eq!(live.configured_default_selector, persisted.model);
    assert_eq!(
        live.aliases.find("preferred").unwrap().model_string,
        persisted.models.model_aliases["preferred"].model_string
    );
    assert_eq!(
        router
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        persisted.models.model_aliases["preferred"].model_string
    );
}

fn exact_context_limits_refresh_alias_precedence_and_preset_rollback(reject_selection: bool) {
    let fixture = Fixture::new(
        "model: first\ncontext_limit: 100k\nmodel_aliases:\n  first:\n    model_string: p/model\n    usage: First\n    context_limit: 20%\n  second:\n    model_string: p/model\n    usage: Second\n",
    );
    let configuration = fixture.load();
    let (routing, router) = routing(registry(&["model"]), &configuration.model, vec![]);
    let coordinator = ModelConfigurationCoordinator::new(configuration, routing, router.clone());
    coordinator.refresh().unwrap();
    let captured = router.resolve("second").unwrap();
    coordinator.set_preset("saved", "first").unwrap();
    coordinator
        .set_context_limit(ContextSize::parse("50k").unwrap())
        .unwrap();
    assert_eq!(
        router
            .resolve("first")
            .unwrap()
            .context_limit()
            .unwrap()
            .to_string(),
        "20%"
    );
    assert_eq!(
        router
            .resolve("second")
            .unwrap()
            .context_limit()
            .unwrap()
            .to_string(),
        "50000"
    );
    assert_eq!(captured.context_limit().unwrap().to_string(), "100000");
    assert_eq!(
        router.resolve("first").unwrap().canonical_model.selector(),
        router.resolve("second").unwrap().canonical_model.selector()
    );
    let mut callbacks = 0;
    if reject_selection {
        let failed = coordinator.select_preset("saved", "second", |_| {
            callbacks += 1;
            Err(CoordinatorError::Provider(Error::InvalidOperation(
                "rejected".into(),
            )))
        });
        let failure = failed.err().unwrap();
        assert!(matches!(
            &failure,
            CoordinatorError::Provider(Error::InvalidOperation(_))
        ));
        assert_eq!(failure.to_string(), "rejected");
        assert_eq!(callbacks, 2);
    } else {
        coordinator
            .select_preset("saved", "second", |_| {
                callbacks += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(callbacks, 1);
    }
    let expected = ContextSize::parse(if reject_selection { "50k" } else { "100k" }).unwrap();
    assert_eq!(
        router.resolve("second").unwrap().context_limit(),
        Some(&expected)
    );
    assert_eq!(
        coordinator.capture_configuration().models.context_limit,
        Some(expected.clone())
    );
    assert_eq!(fixture.load().models.context_limit, Some(expected));
    assert_eq!(
        router
            .resolve("first")
            .unwrap()
            .context_limit()
            .unwrap()
            .to_string(),
        "20%"
    );
}
#[test]
fn aliases_provider_defaults_and_failure_types_use_actual_foundation_mutations() {
    let fixture = Fixture::new(&format!(
        "{INITIAL}provider_model_alias_defaults:\n  p:\n    low_llm: p/low\n    medium_llm: p/medium\n    high_llm: p/high\n    xhigh_llm: p/xhigh\n"
    ));
    let (coordinator, router, _) = fixture.coordinator();
    let configured = coordinator.configure_alias("preferred", "p/new").unwrap();
    assert_eq!(configured.model_string, "p/new");
    assert_eq!(configured.usage, "Primary");
    assert_eq!(
        router
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/new"
    );
    assert_eq!(
        fixture.load().models.model_aliases["preferred"].model_string,
        "p/new"
    );
    let defaults = coordinator.list_provider_defaults(["missing", "p", "p"]);
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0].provider_id, "p");
    let applied = coordinator.apply_provider_defaults("p").unwrap();
    assert_eq!(applied.len(), 4);
    assert_eq!(
        applied
            .iter()
            .map(|alias| alias.name.as_str())
            .collect::<Vec<_>>(),
        vec!["high_llm", "low_llm", "medium_llm", "xhigh_llm"]
    );
    for alias in applied {
        assert_eq!(
            router
                .resolve(&alias.name)
                .unwrap()
                .canonical_model
                .selector(),
            alias.model_string
        );
    }
    assert!(matches!(
        coordinator
            .select_preset("missing", "preferred", |_| Ok(()))
            .err()
            .unwrap(),
        CoordinatorError::MissingPreset(_)
    ));
    assert!(matches!(
        coordinator
            .configure_alias("missing", "p/old")
            .err()
            .unwrap(),
        CoordinatorError::Provider(_)
    ));
}

#[test]
fn validation_and_write_failure_do_not_publish_the_rejected_target() {
    let fixture = Fixture::new(INITIAL);
    let (coordinator, router, routing) = fixture.coordinator();
    let before = routing.capture();
    assert!(matches!(
        coordinator
            .configure_alias("preferred", "q/model")
            .err()
            .unwrap(),
        CoordinatorError::Provider(_)
    ));
    assert_eq!(
        router
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/old"
    );
    assert_eq!(routing.capture().revision, before.revision + 1); // Source Reload publishes the valid refreshed generation first.
    assert_eq!(
        fixture.load().models.model_aliases["preferred"].model_string,
        "p/old"
    );
    let temporary = PathBuf::from(format!("{}.tmp", fixture.path.display()));
    std::fs::create_dir(&temporary).unwrap();
    let failed = coordinator
        .configure_alias("preferred", "p/new")
        .err()
        .unwrap();
    let CoordinatorError::Foundation(error) = &failed else {
        panic!("expected Foundation write error: {failed}");
    };
    assert_eq!(error.kind(), ConfigErrorKind::Io);
    assert_eq!(
        error.to_string(),
        std::fs::write(&temporary, "probe").unwrap_err().to_string()
    );
    assert_eq!(failed.to_string(), error.to_string());
    assert_eq!(
        router
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/old"
    );
    assert_eq!(
        fixture.load().models.model_aliases["preferred"].model_string,
        "p/old"
    );
}

#[test]
fn separate_coordinators_and_direct_setter_share_the_same_path_transaction() {
    let fixture = Fixture::new(INITIAL);
    let (first, _, _) = fixture.coordinator();
    first.set_preset("Work", "preferred").unwrap();
    let (second, _, _) = fixture.coordinator();
    let mut direct = fixture.load();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let select = std::thread::spawn(move || {
        first.select_preset("Work", "preferred", |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let started = started_tx.clone();
    let finished = finished_tx.clone();
    let configure = std::thread::spawn(move || {
        started.send(()).unwrap();
        let result = second.configure_alias("preferred", "p/new");
        finished.send(()).unwrap();
        result
    });
    let setter = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = direct.set_model_alias("preferred", "p/later");
        finished_tx.send(()).unwrap();
        result
    });
    for _ in 0..2 {
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
    assert_eq!(
        finished_rx.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );
    assert!(
        std::fs::read_to_string(&fixture.path)
            .unwrap()
            .contains("p/old")
    );
    release_tx.send(()).unwrap();
    select.join().unwrap().unwrap();
    configure.join().unwrap().unwrap();
    setter.join().unwrap().unwrap();
    for _ in 0..2 {
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
    let persisted = fixture.load();
    assert!(matches!(
        persisted.models.model_aliases["preferred"]
            .model_string
            .as_str(),
        "p/new" | "p/later"
    ));
}

#[test]
fn selection_callback_reenters_same_coordinator_without_deadlock() {
    let fixture = Fixture::new(INITIAL);
    let (coordinator, router, _) = fixture.coordinator();
    coordinator.set_preset("Work", "preferred").unwrap();
    let other = coordinator.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let nested = other.clone();
        let result = other.select_preset("Work", "preferred", move |_| {
            nested.configure_alias("preferred", "p/new")?;
            Ok(())
        });
        sent.send(result.map(|selection| selection.resolved_selection.canonical_model.selector()))
            .unwrap();
    });
    let selected = received
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("same coordinator reentry must not deadlock")
        .unwrap();
    worker.join().unwrap();
    assert_eq!(selected, "p/old");
    assert_eq!(
        router
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/new"
    );
    assert_eq!(
        fixture.load().models.model_aliases["preferred"].model_string,
        "p/new"
    );
}

#[test]
fn selection_callback_reenters_same_path_separate_handle_direct_setter() {
    let fixture = Fixture::new(INITIAL);
    let (coordinator, _, _) = fixture.coordinator();
    coordinator.set_preset("Work", "preferred").unwrap();
    let mut separate = fixture.load();
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = coordinator.select_preset("Work", "preferred", move |_| {
            separate.set_model_alias("preferred", "p/new")?;
            Ok(())
        });
        sent.send(result.map(|selection| selection.resolved_selection.canonical_model.selector()))
            .unwrap();
    });
    assert_eq!(
        received
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("same path direct setter reentry must not deadlock")
            .unwrap(),
        "p/old"
    );
    worker.join().unwrap();
    assert_eq!(
        fixture.load().models.model_aliases["preferred"].model_string,
        "p/new"
    );
}

#[test]
fn context_limits_refresh_alias_precedence_and_preset_rollback_false() {
    exact_context_limits_refresh_alias_precedence_and_preset_rollback(false);
}
#[test]
fn context_limits_refresh_alias_precedence_and_preset_rollback_true() {
    exact_context_limits_refresh_alias_precedence_and_preset_rollback(true);
}
