use super::{
    common::{array, boolean, coordinator_error, error, resolved, snapshot, text},
    registry::no_refresh_registry,
};
use parrot_domain::values::context::ContextSize;
use parrot_foundation::config::Configuration;
use parrot_provider::{
    routing::{
        aliases::{ModelAliasCatalog, ModelRouter, ModelRouting},
        configuration::{CoordinatorError, ModelConfigurationCoordinator, definitions_from_config},
    },
    sessions::Error,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

struct Files {
    directory: PathBuf,
    path: PathBuf,
    predefined: PathBuf,
}
impl Files {
    fn new(content: &str) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(".tmp");
        std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let directory = loop {
            let path = parent.join(format!(
                "t32-projector-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        };
        let files = Self {
            path: directory.join("config.yaml"),
            predefined: directory.join("predefined.yaml"),
            directory,
        };
        std::fs::write(&files.path, content).map_err(|e| e.to_string())?;
        Ok(files)
    }
    fn load(&self) -> Result<Configuration, String> {
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
        Configuration::load(&self.path, &self.predefined, &environment, |_| Ok(()))
            .map_err(|e| e.to_string())
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub async fn run(input: &Value) -> Result<Value, String> {
    let files = Files::new(text(input, "yaml"))?;
    let configuration = files.load()?;
    let registry = match no_refresh_registry(input) {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error":error(&e)})),
    };
    let definitions = match definitions_from_config(&configuration.models.model_aliases) {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error":coordinator_error(&e)})),
    };
    let catalog = match ModelAliasCatalog::new(registry.clone(), definitions) {
        Ok(v) => Arc::new(v),
        Err(e) => return Ok(json!({"error":error(&e)})),
    };
    let routing = Arc::new(ModelRouting::new(catalog, &configuration.model));
    let mut initial = (*routing.capture()).clone();
    initial.context_limit = configuration.models.context_limit.clone();
    routing.publish(Arc::new(initial));
    let router = Arc::new(ModelRouter::new(registry, routing.clone()));
    let coordinator = ModelConfigurationCoordinator::new(configuration, routing, router);
    let mut observations = Vec::new();
    for operation in array(input, "operations") {
        let kind = text(operation, "kind");
        let mut callbacks = Vec::new();
        let result: Result<Value, CoordinatorError> = match kind {
            "alias" => coordinator
                .configure_alias(text(operation, "name"), text(operation, "target"))
                .map(|a| Value::String(a.model_string)),
            "defaults" => coordinator
                .apply_provider_defaults(text(operation, "provider_id"))
                .map(|aliases| {
                    json!(
                        aliases
                            .iter()
                            .map(|a| json!({"name":a.name,"target":a.model_string}))
                            .collect::<Vec<_>>()
                    )
                }),
            "set_preset" => coordinator
                .set_preset(text(operation, "name"), text(operation, "selected"))
                .map(|p| Value::String(p.model)),
            "select_preset" => coordinator
                .select_preset(
                    text(operation, "name"),
                    text(operation, "selected"),
                    |value| {
                        callbacks.push(resolved(&value));
                        if boolean(operation, "fail_apply", false) && callbacks.len() == 1 {
                            return Err(CoordinatorError::Provider(Error::InvalidOperation(
                                "synthetic apply failure".into(),
                            )));
                        }
                        Ok(())
                    },
                )
                .map(|selection| resolved(&selection.resolved_selection)),
            "context" => {
                let limit =
                    ContextSize::parse(text(operation, "value")).map_err(|e| e.to_string())?;
                coordinator.set_context_limit(limit).map(|s| snapshot(&s))
            }
            "rewrite" => {
                std::fs::write(&files.path, text(operation, "yaml")).map_err(|e| e.to_string())?;
                coordinator.refresh().map(|s| snapshot(&s))
            }
            "refresh" => coordinator.refresh().map(|s| snapshot(&s)),
            unknown => return Err(format!("unknown configuration fixture operation {unknown}")),
        };
        let (result, operation_error) = match result {
            Ok(result) => (result, Value::Null),
            Err(e) => (Value::Null, coordinator_error(&e)),
        };
        let configuration = coordinator.capture_configuration();
        let configured_aliases = configuration
            .models
            .model_aliases
            .iter()
            .map(|(name, a)| (name.clone(), Value::String(a.model_string.clone())))
            .collect::<serde_json::Map<_, _>>();
        let presets = configuration
            .models
            .model_presets
            .iter()
            .map(|(name, p)| {
                (
                    name.clone(),
                    json!({"model":p.model,"aliases":p.model_aliases}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        observations.push(json!({"kind":kind,"result":result,"error":operation_error,"callbacks":callbacks,"snapshot":snapshot(&coordinator.capture_routing()),"configured_model":configuration.model,"configured_aliases":configured_aliases,"presets":presets,"file_text":std::fs::read_to_string(&files.path).map_err(|e|e.to_string())?}));
    }
    Ok(json!({"operations":observations}))
}
