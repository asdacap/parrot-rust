use super::fixtures;
use parrot_foundation::config::{
    self, ConfigError, ConfigErrorKind, Configuration, node::Node, templates, yaml,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn temporary() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tmp")
        .join(format!(
            "t11-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn config_error_kind_preserves_validation_message_and_debug() {
    let error = fixtures::fixture_configuration("model_aliases: []\n").unwrap_err();
    assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
    assert_eq!(error.0, "model_aliases must be a mapping");
    assert_eq!(error.to_string(), error.0);
    assert_eq!(
        format!("{error:?}"),
        "ConfigError(\"model_aliases must be a mapping\")"
    );
    assert_eq!(error.clone(), error);
}

#[test]
fn config_error_kind_preserves_io_message_and_permission_denied_category() {
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access refused");
    let message = io.to_string();
    let error = ConfigError::from(io);
    assert_eq!(error.kind(), ConfigErrorKind::Io);
    assert_eq!(error.0, message);
    assert_eq!(error.to_string(), message);
    assert_eq!(format!("{error:?}"), "ConfigError(\"access refused\")");
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(error.clone(), error);
    assert_ne!(error, ConfigError::invalid_data(message));
}

#[test]
fn config_error_kind_retains_actual_setter_write_failure_without_publication() {
    let context = super::support::Context::new();
    let mut configuration = context.configured("model: p/old\n").unwrap();
    let before = context.saved();
    let temporary = PathBuf::from(format!("{}.tmp", context.path.display()));
    std::fs::create_dir(&temporary).unwrap();
    let expected = std::fs::write(&temporary, "probe").unwrap_err().to_string();
    let error = configuration.set_model("p/new").unwrap_err();
    assert_eq!(error.kind(), ConfigErrorKind::Io);
    assert_eq!(error.0, expected);
    assert_eq!(error.to_string(), expected);
    assert_eq!(configuration.model, "p/old");
    assert_eq!(context.saved(), before);
}

#[test]
fn config_error_kind_preserves_predefined_copy_sanitized_validation() {
    let context = super::support::Context::new();
    std::fs::create_dir(&context.predefined).unwrap();
    let error = context.load().unwrap_err();
    assert_eq!(error.kind(), ConfigErrorKind::InvalidData);
    assert_eq!(error.0, "unable to write predefined configuration");
    assert_eq!(
        error.to_string(),
        "unable to write predefined configuration"
    );
    assert_eq!(
        format!("{error:?}"),
        "ConfigError(\"unable to write predefined configuration\")"
    );
}

#[test]
fn full_load_requires_and_propagates_every_template_validator_call() {
    let mut calls = Vec::new();
    let error = Configuration::from_content("", &fixtures::environment(), |spec| {
        calls.push(spec.path.clone());
        Err(ConfigError::invalid_data(format!(
            "{}: injected validator rejection",
            spec.path
        )))
    })
    .unwrap_err();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        error.0,
        format!("{}: injected validator rejection", calls[0])
    );
    let root =
        yaml::parse_root(&config::configuration::source_predefined_content().unwrap()).unwrap();
    let mut expected = BTreeMap::new();
    templates::read_templates(&root, &mut |spec| {
        expected.insert(spec.path.clone(), spec.clone());
        Ok(())
    })
    .unwrap();
    let mut visited = BTreeMap::new();
    let configuration = Configuration::from_content("", &fixtures::environment(), |spec| {
        assert_eq!(expected.get(&spec.path), Some(spec));
        visited.insert(spec.path.clone(), spec.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(visited, expected);
    assert_eq!(configuration.prompt_templates.len(), visited.len());
}

#[test]
fn approved_legacy_flag_is_default_true_and_does_not_change_resource_values() {
    let default = fixtures::fixture_configuration("").unwrap();
    assert!(default.legacy_path_read_fallback);
    let disabled = fixtures::fixture_configuration("legacy_path_read_fallback: false\n").unwrap();
    assert!(!disabled.legacy_path_read_fallback);
    assert_eq!(default.models, disabled.models);
    assert_eq!(default.runtime.snapshot(), disabled.runtime.snapshot());
    assert_eq!(default.system_prompts, disabled.system_prompts);
    assert_eq!(default.prompt_templates, disabled.prompt_templates);
    for invalid in ["null", "TRUE", "[]", "0"] {
        assert_eq!(
            fixtures::fixture_configuration(&format!("legacy_path_read_fallback: {invalid}\n"))
                .unwrap_err()
                .0,
            "legacy_path_read_fallback must be true or false"
        );
    }
}

#[test]
fn copied_defaults_are_refreshed_but_unknown_user_yaml_is_not_overwritten_on_load() {
    let root = temporary();
    let path = root.join("config.yaml");
    let predefined = root.join("defaults.yaml");
    let user = "theme: dark\nunknown:\n  keep: 'true'\n";
    std::fs::write(&path, user).unwrap();
    std::fs::write(&predefined, "malformed: [").unwrap();
    let mut configuration = fixtures::fixture_load(&path, &predefined).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
    assert!(
        std::fs::read_to_string(&predefined)
            .unwrap()
            .contains("legacy_path_read_fallback: true")
    );
    configuration.set_model("provider/new").unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    let parsed = yaml::parse_root(&saved).unwrap();
    assert_eq!(parsed.get("theme").and_then(Node::as_str), Some("dark"));
    assert_eq!(
        parsed
            .get("unknown")
            .and_then(|v| v.get("keep"))
            .and_then(Node::as_str),
        Some("true")
    );
    assert!(parsed.get("prompt_templates").is_none());
}

#[test]
fn routing_reload_ignores_unrelated_provider_validation_and_sync_does_not_change_sandbox() {
    let root = temporary();
    let path = root.join("config.yaml");
    let predefined = root.join("defaults.yaml");
    std::fs::write(&path, "model: p/old\nsandbox:\n  enabled: false\n").unwrap();
    let mut configuration = fixtures::fixture_load(&path, &predefined).unwrap();
    std::fs::write(&path,"model: p/new\nsandbox:\n  enabled: true\nproviders:\n  bad:\n    disable_websocket: invalid\n").unwrap();
    let refreshed = configuration.reload_model_configuration().unwrap();
    assert_eq!(refreshed.model, "p/new");
    assert!(refreshed.sandbox_enabled);
    configuration.synchronize_model_configuration(&refreshed);
    assert_eq!(configuration.model, "p/new");
    assert!(!configuration.runtime.sandbox_enabled);
}

#[test]
fn configuration_instances_share_the_source_style_read_modify_write_lock() {
    let root = temporary();
    let path = root.join("config.yaml");
    let predefined = root.join("defaults.yaml");
    std::fs::write(&path, "unknown: retained\n").unwrap();
    let instances: Vec<_> = (0..8)
        .map(|_| fixtures::fixture_load(&path, &predefined).unwrap())
        .collect();
    let handles: Vec<_> = instances
        .into_iter()
        .enumerate()
        .map(|(index, mut configuration)| {
            std::thread::spawn(move || {
                let alias = ["low_llm", "medium_llm", "high_llm", "xhigh_llm"][index % 4];
                configuration
                    .set_model_alias(alias, &format!("provider/model-{index}"))
                    .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let root = yaml::parse_root(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root.get("unknown").and_then(Node::as_str), Some("retained"));
    for alias in ["low_llm", "medium_llm", "high_llm", "xhigh_llm"] {
        assert!(
            root.get("model_aliases")
                .unwrap()
                .get(alias)
                .unwrap()
                .get("model_string")
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("provider/model-")
        );
    }
}
