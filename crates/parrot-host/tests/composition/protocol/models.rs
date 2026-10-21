//! Offline host protocol tests using the actual coordinator, registry and router.
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as proto,
    values::context::ContextSize,
};
use parrot_foundation::config::{ConfigErrorKind, Configuration};
use parrot_host::composition::protocol::{RpcCode, RpcError, models::ModelOperations};
use parrot_provider::{
    catalogs::{Model, ModelVariant},
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        configuration::{CoordinatorError, ModelConfigurationCoordinator, definitions_from_config},
        registry::ProviderRegistry,
    },
    sessions::{
        Content, Error, EventStream, Provider, Request, Session,
        decorators::{
            SubscriptionUsage, UsageCredits, UsageReporter, UsageReportingProvider, UsageWindow,
        },
    },
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

struct Cancel(bool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending::<()>().await
            }
        })
    }
}
const ACTIVE: Cancel = Cancel(false);
const CANCELLED: Cancel = Cancel(true);

struct FixtureProvider {
    id: &'static str,
    credential: bool,
    models: Vec<Model>,
    refreshes: Arc<AtomicUsize>,
}
impl Provider for FixtureProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn call(&self, _: Request) -> std::result::Result<Box<dyn EventStream>, Error> {
        panic!("no live calls")
    }
    fn has_credential<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<bool, Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(self.credential)
            }
        })
    }
    fn seed_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<Vec<Model>, Error>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(self.models.clone()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> std::result::Result<Arc<dyn Session>, Error> {
        panic!("no sessions")
    }
}

struct Reporter(std::result::Result<SubscriptionUsage, Error>);
impl UsageReporter for Reporter {
    fn usage<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, std::result::Result<SubscriptionUsage, Error>> {
        Box::pin(async move { self.0.clone() })
    }
}

struct Fixture {
    directory: PathBuf,
    operations: ModelOperations,
    coordinator: Arc<ModelConfigurationCoordinator>,
    refreshes: Arc<AtomicUsize>,
}
const INITIAL: &str = "model: preferred\ncontext_limit: 50%\nmodel_aliases:\n  preferred:\n    model_string: p/old\n    usage: Primary\n    context_limit: 2k\nprovider_model_alias_defaults:\n  p:\n    low_llm: p/low\n    medium_llm: p/medium\n    high_llm: p/high\n    xhigh_llm: p/xhigh\n  unavailable:\n    low_llm: unavailable/low\n    medium_llm: unavailable/medium\n    high_llm: unavailable/high\n    xhigh_llm: unavailable/xhigh\n";
impl Fixture {
    fn new(reporter: Option<std::result::Result<SubscriptionUsage, Error>>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "t62-models-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.yaml");
        std::fs::write(&path, INITIAL).unwrap();
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
                directory.join(name).to_string_lossy().into_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
        let configuration = Configuration::load(
            &path,
            &directory.join("predefined.yaml"),
            &environment,
            |_| Ok(()),
        )
        .unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let mut model = Model::new("old", "p");
        model.capabilities.variants.push(ModelVariant {
            name: "deep".into(),
            reasoning_effort: "high".into(),
        });
        let mut provider: Arc<dyn Provider> = Arc::new(FixtureProvider {
            id: "p",
            credential: true,
            models: vec![model.clone()],
            refreshes: refreshes.clone(),
        });
        if let Some(reporter) = reporter {
            provider = Arc::new(UsageReportingProvider::new(
                provider,
                Arc::new(Reporter(reporter)),
            ));
        }
        let unavailable = Arc::new(FixtureProvider {
            id: "unavailable",
            credential: false,
            models: vec![Model::new("hidden", "unavailable")],
            refreshes: refreshes.clone(),
        });
        let registry = Arc::new(
            ProviderRegistry::new(
                vec![unavailable, provider],
                BTreeMap::from([("p".into(), vec![model])]),
            )
            .unwrap(),
        );
        let catalog = Arc::new(
            ModelAliasCatalog::new(
                registry.clone(),
                definitions_from_config(&configuration.models.model_aliases).unwrap(),
            )
            .unwrap(),
        );
        let routing = Arc::new(ModelRouting::new(catalog, configuration.model.clone()));
        let router = Arc::new(ModelRouter::new(registry.clone(), routing.clone()));
        let coordinator = Arc::new(ModelConfigurationCoordinator::new(
            configuration,
            routing,
            router.clone(),
        ));
        let operations = ModelOperations::new(coordinator.clone(), registry, router);
        Self {
            directory,
            operations,
            coordinator,
            refreshes,
        }
    }
    fn configure(&self, model: &str) {
        self.operations
            .configure_model_alias(&proto::ConfigureModelAliasRequest {
                name: "preferred".into(),
                model_string: model.into(),
                ..Default::default()
            })
            .unwrap();
    }
    fn save(&self, name: &str, model: &str) -> proto::SetModelPresetResponse {
        self.operations
            .set_model_preset(
                &proto::SetModelPresetRequest {
                    name: name.into(),
                    ..Default::default()
                },
                model,
                &ACTIVE,
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[tokio::test]
async fn models_refresh_real_registry_filter_credentials_and_preserve_variants() {
    let fixture = Fixture::new(None);
    let response = fixture.operations.list_models(&ACTIVE).await.unwrap();
    assert_eq!(response.models.len(), 1);
    assert_eq!(response.models[0].id, "old");
    assert_eq!(response.models[0].provider_id, "p");
    assert_eq!(response.models[0].variants[0].name, "deep");
    assert_eq!(response.models[0].variants[0].reasoning_effort, "high");
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .operations
            .list_models(&CANCELLED)
            .await
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
}

#[test]
fn alias_configuration_maps_protocol_and_updates_the_actual_router() {
    let fixture = Fixture::new(None);
    let before = fixture.operations.resolve("preferred").unwrap();
    fixture.configure("p/new");
    let aliases = fixture.operations.list_model_aliases();
    let preferred = aliases
        .aliases
        .iter()
        .find(|alias| alias.name == "preferred")
        .unwrap();
    assert_eq!(preferred.model_string, "p/new");
    assert_eq!(preferred.usage, "Primary");
    assert_eq!(before.canonical_model.selector(), "p/old");
    assert_eq!(
        fixture
            .operations
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/new"
    );
    let request = proto::ConfigureModelAliasRequest {
        name: "missing".into(),
        model_string: "p/new".into(),
        ..Default::default()
    };
    assert_eq!(
        fixture
            .operations
            .configure_model_alias(&request)
            .unwrap_err()
            .code,
        RpcCode::InvalidArgument
    );
    let request = proto::ConfigureModelAliasRequest {
        name: "preferred".into(),
        model_string: "missing/new".into(),
        ..Default::default()
    };
    assert_eq!(
        fixture
            .operations
            .configure_model_alias(&request)
            .unwrap_err()
            .code,
        RpcCode::InvalidArgument
    );
    assert_eq!(
        fixture
            .operations
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/new"
    );
}

#[tokio::test]
async fn provider_defaults_require_available_provider_and_persist_all_four_aliases() {
    let fixture = Fixture::new(None);
    let defaults = fixture
        .operations
        .list_provider_model_alias_defaults(&ACTIVE)
        .await
        .unwrap();
    assert_eq!(
        defaults
            .providers
            .iter()
            .map(|provider| provider.provider_id.as_str())
            .collect::<Vec<_>>(),
        ["p"]
    );
    let request = proto::ApplyProviderModelAliasDefaultsRequest {
        provider_id: "unavailable".into(),
        ..Default::default()
    };
    let error = fixture
        .operations
        .apply_provider_model_alias_defaults(&request, &ACTIVE)
        .await
        .unwrap_err();
    assert_eq!(error.code, RpcCode::InvalidArgument);
    assert_eq!(
        error.message,
        "model alias defaults: provider \"unavailable\" is not available"
    );
    let request = proto::ApplyProviderModelAliasDefaultsRequest {
        provider_id: "p".into(),
        ..Default::default()
    };
    let response = fixture
        .operations
        .apply_provider_model_alias_defaults(&request, &ACTIVE)
        .await
        .unwrap();
    assert_eq!(response.aliases.len(), 4);
    assert_eq!(
        fixture
            .operations
            .resolve("high_llm")
            .unwrap()
            .canonical_model
            .selector(),
        "p/high"
    );
    assert_eq!(
        fixture
            .coordinator
            .capture_configuration()
            .models
            .model_aliases["low_llm"]
            .model_string,
        "p/low"
    );
}

#[test]
fn presets_snapshot_context_maps_and_apply_actual_selection() {
    let fixture = Fixture::new(None);
    let saved = fixture.save("Work", "preferred");
    let preset = saved.preset.as_ref().unwrap();
    assert_eq!(preset.model, "preferred");
    assert_eq!(preset.context_limit, "50%");
    assert_eq!(preset.model_alias_context_limits["preferred"], "2000");
    fixture.configure("p/new");
    let mut updates = Vec::new();
    let (mapped, selected) = fixture
        .operations
        .select_model_preset(
            &proto::SelectModelPresetRequest {
                name: "Work".into(),
                ..Default::default()
            },
            "preferred",
            |selection| {
                updates.push(selection.canonical_model.selector());
                Ok(())
            },
            &ACTIVE,
        )
        .unwrap();
    assert_eq!(updates, ["p/old"]);
    assert_eq!(mapped, *preset);
    assert_eq!(selected.preset.model, "preferred");
    assert_eq!(
        selected.resolved_selection.canonical_model.selector(),
        "p/old"
    );
    assert_eq!(
        fixture
            .operations
            .list_model_presets(&ACTIVE)
            .unwrap()
            .presets,
        vec![mapped]
    );
    let missing = fixture
        .operations
        .select_model_preset(
            &proto::SelectModelPresetRequest {
                name: "Missing".into(),
                ..Default::default()
            },
            "preferred",
            |_| panic!("missing preset cannot update session"),
            &ACTIVE,
        )
        .unwrap_err();
    assert_eq!(missing.code, RpcCode::NotFound);
}

#[test]
fn preset_callback_failure_restores_actual_selection_and_routing() {
    let fixture = Fixture::new(None);
    fixture.save("Work", "preferred");
    fixture.configure("p/new");
    let mut updates = Vec::new();
    let error = fixture
        .operations
        .select_model_preset(
            &proto::SelectModelPresetRequest {
                name: "Work".into(),
                ..Default::default()
            },
            "preferred",
            |selection| {
                updates.push(selection.canonical_model.selector());
                if updates.len() == 1 {
                    Err(Error::Io("synthetic session persistence failure".into()).into())
                } else {
                    Ok(())
                }
            },
            &ACTIVE,
        )
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert!(error.message.starts_with("failed to select model preset:"));
    assert_eq!(updates, ["p/old", "p/new"]);
    assert_eq!(
        fixture
            .operations
            .resolve("preferred")
            .unwrap()
            .canonical_model
            .selector(),
        "p/new"
    );
    assert_eq!(
        fixture
            .coordinator
            .capture_configuration()
            .models
            .model_aliases["preferred"]
            .model_string,
        "p/new"
    );
}

// These four handlers have explicit IOException/UnauthorizedAccessException
// catches in pinned ParrotService. SetContextLimit deliberately is not included.
fn fail_configuration_write(fixture: &Fixture) {
    std::fs::create_dir(fixture.directory.join("config.yaml.tmp")).unwrap();
}
fn assert_persistence_failure(error: parrot_host::composition::protocol::RpcError, context: &str) {
    assert_eq!(error.code, RpcCode::Internal);
    assert!(!error.is_unhandled_io());
    assert!(
        error.message.starts_with(&format!("{context}: ")),
        "{error}"
    );
}
#[test]
fn configure_alias_classifies_actual_configuration_io_as_internal() {
    let fixture = Fixture::new(None);
    fail_configuration_write(&fixture);
    let error = fixture
        .operations
        .configure_model_alias(&proto::ConfigureModelAliasRequest {
            name: "preferred".into(),
            model_string: "p/new".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert_persistence_failure(error, "failed to persist model alias");
}
#[tokio::test]
async fn apply_alias_defaults_classifies_actual_configuration_io_as_internal() {
    let fixture = Fixture::new(None);
    fail_configuration_write(&fixture);
    let error = fixture
        .operations
        .apply_provider_model_alias_defaults(
            &proto::ApplyProviderModelAliasDefaultsRequest {
                provider_id: "p".into(),
                ..Default::default()
            },
            &ACTIVE,
        )
        .await
        .unwrap_err();
    assert_persistence_failure(error, "failed to persist model aliases");
}
#[test]
fn save_preset_classifies_actual_configuration_io_as_internal() {
    let fixture = Fixture::new(None);
    fail_configuration_write(&fixture);
    let error = fixture
        .operations
        .set_model_preset(
            &proto::SetModelPresetRequest {
                name: "Work".into(),
                ..Default::default()
            },
            "preferred",
            &ACTIVE,
        )
        .unwrap_err();
    assert_persistence_failure(error, "failed to persist model preset");
}
#[test]
fn select_preset_classifies_actual_configuration_io_before_session_callback() {
    let fixture = Fixture::new(None);
    fixture.save("Work", "preferred");
    fail_configuration_write(&fixture);
    let error = fixture
        .operations
        .select_model_preset(
            &proto::SelectModelPresetRequest {
                name: "Work".into(),
                ..Default::default()
            },
            "preferred",
            |_| panic!("failed configuration reload cannot update the hosted selection"),
            &ACTIVE,
        )
        .unwrap_err();
    assert_persistence_failure(error, "failed to select model preset");
}

#[test]
fn context_limit_io_preserves_unhandled_internal_message_and_client_errors() {
    let fixture = Fixture::new(None);
    let request = |text: &str| proto::SetContextLimitRequest {
        context_limit: text.into(),
        ..Default::default()
    };
    let invalid_inputs = ["", "0", "101%", "garbage"];
    let expected = invalid_inputs.map(|text| {
        fixture
            .operations
            .set_context_limit(&request(text), "preferred", &ACTIVE)
            .unwrap_err()
    });
    fail_configuration_write(&fixture);
    let cause = fixture
        .coordinator
        .set_context_limit(ContextSize::parse("100k").unwrap())
        .unwrap_err();
    let CoordinatorError::Foundation(cause) = cause else {
        panic!("expected the actual typed configuration I/O failure, got {cause}");
    };
    assert_eq!(cause.kind(), ConfigErrorKind::Io);
    let error = fixture
        .operations
        .set_context_limit(&request("100k"), "preferred", &ACTIVE)
        .unwrap_err();
    assert_eq!(error.code, RpcCode::Internal);
    assert_eq!(error.message, cause.to_string());
    assert!(error.is_unhandled_io());
    assert!(!RpcError::new(RpcCode::Internal, cause.to_string()).is_unhandled_io());
    assert!(!RpcError::cancelled().is_unhandled_io());
    for (text, expected) in invalid_inputs.into_iter().zip(expected) {
        assert_eq!(expected.code, RpcCode::InvalidArgument);
        assert!(!expected.is_unhandled_io());
        assert_eq!(
            fixture
                .operations
                .set_context_limit(&request(text), "preferred", &ACTIVE)
                .unwrap_err(),
            expected
        );
    }
    assert_eq!(
        fixture
            .operations
            .set_context_limit(&request("100k"), "preferred", &CANCELLED)
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );

    let invalid_data = Fixture::new(None);
    std::fs::write(
        invalid_data.directory.join("config.yaml"),
        "model_aliases: []\n",
    )
    .unwrap();
    let error = invalid_data
        .operations
        .set_context_limit(&request("100k"), "preferred", &ACTIVE)
        .unwrap_err();
    assert_eq!(error.code, RpcCode::InvalidArgument);
    assert!(!error.is_unhandled_io());
    assert!(error.message.contains("model_aliases must be a mapping"));
}

#[test]
fn context_limit_canonicalizes_input_and_reports_only_explicit_current_alias_override() {
    let fixture = Fixture::new(None);
    let request = proto::SetContextLimitRequest {
        context_limit: "3k".into(),
        ..Default::default()
    };
    let response = fixture
        .operations
        .set_context_limit(&request, "preferred", &ACTIVE)
        .unwrap();
    assert_eq!(response.context_limit, "3000");
    assert!(response.alias_override);
    assert!(
        !fixture
            .operations
            .set_context_limit(&request, "", &ACTIVE)
            .unwrap()
            .alias_override
    );
    assert!(
        !fixture
            .operations
            .set_context_limit(&request, "p/old", &ACTIVE)
            .unwrap()
            .alias_override
    );
    assert_eq!(
        fixture
            .coordinator
            .capture_routing()
            .context_limit
            .as_ref()
            .unwrap()
            .to_string(),
        "3000"
    );
    for text in ["", "0", "101%", "garbage"] {
        let request = proto::SetContextLimitRequest {
            context_limit: text.into(),
            ..Default::default()
        };
        let error = fixture
            .operations
            .set_context_limit(&request, "preferred", &ACTIVE)
            .unwrap_err();
        assert_eq!(error.code, RpcCode::InvalidArgument);
        if text.is_empty() {
            assert_eq!(error.message, "context limit must not be empty");
        }
    }
}

#[test]
fn cancellation_precedes_preset_and_context_side_effects() {
    let fixture = Fixture::new(None);
    assert_eq!(
        fixture
            .operations
            .list_model_presets(&CANCELLED)
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
    assert_eq!(
        fixture
            .operations
            .set_model_preset(
                &proto::SetModelPresetRequest {
                    name: "Work".into(),
                    ..Default::default()
                },
                "preferred",
                &CANCELLED
            )
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
    assert_eq!(
        fixture
            .operations
            .select_model_preset(
                &proto::SelectModelPresetRequest::default(),
                "preferred",
                |_| panic!("cancelled callback"),
                &CANCELLED
            )
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
    assert_eq!(
        fixture
            .operations
            .set_context_limit(
                &proto::SetContextLimitRequest::default(),
                "preferred",
                &CANCELLED
            )
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
    assert!(fixture.coordinator.capture_presets().is_empty());
}

#[test]
fn invalid_preset_and_erased_foundation_validation_are_invalid_argument() {
    let fixture = Fixture::new(None);
    let request = proto::SetModelPresetRequest {
        name: " bad ".into(),
        ..Default::default()
    };
    assert_eq!(
        fixture
            .operations
            .set_model_preset(&request, "preferred", &ACTIVE)
            .unwrap_err()
            .code,
        RpcCode::InvalidArgument
    );
    std::fs::write(fixture.directory.join("config.yaml"), "model_aliases: []\n").unwrap();
    let request = proto::ConfigureModelAliasRequest {
        name: "preferred".into(),
        model_string: "p/new".into(),
        ..Default::default()
    };
    let error = fixture
        .operations
        .configure_model_alias(&request)
        .unwrap_err();
    assert_eq!(error.code, RpcCode::InvalidArgument);
    assert!(error.message.contains("model_aliases must be a mapping"));
}

#[tokio::test]
async fn usage_formats_windows_and_credit_balance_with_source_wall_clock() {
    let fixture = Fixture::new(Some(Ok(SubscriptionUsage {
        primary_window: Some(UsageWindow {
            used_percent: 12.5,
            reset_at: "2026-10-19T15:20:30.123+02:00".into(),
            limit_window_seconds: 100,
        }),
        secondary_window: Some(UsageWindow {
            used_percent: 41.2,
            reset_at: "2026-10-20T01:02:03Z".into(),
            limit_window_seconds: 200,
        }),
        credits: Some(UsageCredits {
            has_credits: true,
            balance: "12.340".into(),
        }),
        ..Default::default()
    })));
    let resolved = fixture.operations.resolve("preferred").unwrap();
    assert_eq!(
        fixture
            .operations
            .usage_lines(&resolved, &ACTIVE)
            .await
            .unwrap(),
        [
            "Primary window: 13% used, resets 2026-10-19T15:20:30Z",
            "Secondary window: 41% used, resets 2026-10-20T01:02:03Z",
            "Credits: 12.340",
        ]
    );
}

#[tokio::test]
async fn usage_handles_unsupported_empty_failure_and_caller_cancellation() {
    let fixture = Fixture::new(None);
    assert_eq!(
        fixture
            .operations
            .usage_lines(&fixture.operations.resolve("preferred").unwrap(), &ACTIVE)
            .await
            .unwrap(),
        ["Provider does not report usage"]
    );
    for reporter in [
        Ok(SubscriptionUsage::default()),
        Ok(SubscriptionUsage {
            credits: Some(UsageCredits {
                has_credits: false,
                balance: "10".into(),
            }),
            ..Default::default()
        }),
        Err(Error::Io("synthetic".into())),
        Err(Error::Cancelled),
    ] {
        let fixture = Fixture::new(Some(reporter));
        assert_eq!(
            fixture
                .operations
                .usage_lines(&fixture.operations.resolve("preferred").unwrap(), &ACTIVE)
                .await
                .unwrap(),
            ["Usage unavailable"]
        );
    }
    let fixture = Fixture::new(Some(Err(Error::Cancelled)));
    assert_eq!(
        fixture
            .operations
            .usage_lines(
                &fixture.operations.resolve("preferred").unwrap(),
                &CANCELLED
            )
            .await
            .unwrap_err()
            .code,
        RpcCode::Cancelled
    );
}
