use super::models::{
    self, ModelPresetConfig, ModelPresetContextLimits, ModelSettings, ProviderModelAliasDefaults,
};
use super::node::{Kind, Node, boolean, scalar_or_empty};
use super::runtime::{self, RuntimeSettings};
use super::templates::{self, TemplateSpec};
use super::{ConfigError, Result, yaml};
use parrot_domain::values::config::{PathPlatform, PlatformPath};
use parrot_domain::values::context::ContextSize;
use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

const RESOURCES: [(&str, &str); 4] = [
    (
        "Config/predefined_config.system_prompts.yaml",
        include_str!("../../../../assets/config/predefined_config.system_prompts.minijinja.yaml"),
    ),
    (
        "Config/predefined_config.agent_tasks.yaml",
        include_str!("../../../../assets/config/predefined_config.agent_tasks.yaml"),
    ),
    (
        "Config/predefined_config.profiles.yaml",
        include_str!("../../../../assets/config/predefined_config.profiles.yaml"),
    ),
    (
        "Config/predefined_config.runtime.yaml",
        include_str!("../../../../assets/config/predefined_config.runtime.yaml"),
    ),
];

pub fn source_predefined_content() -> Result<String> {
    yaml::compose(
        &RESOURCES
            .iter()
            .map(|(name, text)| (*name, Some(*text)))
            .collect::<Vec<_>>(),
    )
}

pub fn predefined_content() -> Result<String> {
    let mut sources: Vec<_> = RESOURCES
        .iter()
        .map(|(name, text)| (*name, Some(*text)))
        .collect();
    sources.push((
        "approved/parrotrust.yaml",
        Some("legacy_path_read_fallback: true\n"),
    ));
    yaml::compose(&sources)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelConfigurationSnapshot {
    pub model: String,
    pub context_limit: Option<ContextSize>,
    pub sandbox_enabled: bool,
    pub sandbox_dev_bind: Vec<String>,
    pub model_aliases: BTreeMap<String, models::ModelAliasConfig>,
    pub model_presets: BTreeMap<String, ModelPresetConfig>,
}

#[derive(Clone, Debug)]
pub struct Configuration {
    pub model: String,
    pub models: ModelSettings,
    pub runtime: RuntimeSettings,
    pub system_prompts: BTreeMap<String, String>,
    pub prompt_templates: BTreeMap<String, TemplateSpec>,
    pub legacy_path_read_fallback: bool,
    pub skills_generation: i64,
    path: Option<PathBuf>,
    write_lock: Arc<ModelConfigurationLock>,
}

impl Configuration {
    /// Pure layering/validation. The caller must supply completed scalar/Scriban validation.
    pub fn from_content<F>(
        content: &str,
        environment: &BTreeMap<String, String>,
        mut validator: F,
    ) -> Result<Self>
    where
        F: FnMut(&TemplateSpec) -> Result<()>,
    {
        let user_root = yaml::parse_root(content)?;
        yaml::validate_replace_tags(&user_root)?;
        let mut root = yaml::parse_root(&predefined_content()?)?;
        yaml::merge(&mut root, &user_root)?;
        // Source validates profiles before constructing the template catalog and settings.
        runtime::validate_profiles(&root, environment)?;
        let prompt_templates = templates::read_templates(&root, &mut validator)?;
        let _ = models::read_context_limit(&root, "context_limit", "context_limit")?;
        let system_prompts = templates::read_system_prompts(&root)?;
        if let Some(value) = root.get("inline_diff") {
            let _ = boolean(Some(value), "inline_diff")?;
        }
        let models = ModelSettings::read(&root)?;
        let runtime = RuntimeSettings::read(&root, &user_root, environment)?;
        let legacy_path_read_fallback = boolean(
            root.get("legacy_path_read_fallback"),
            "legacy_path_read_fallback",
        )?;
        Ok(Self {
            model: scalar_or_empty(&root, "model"),
            models,
            runtime,
            system_prompts,
            prompt_templates,
            legacy_path_read_fallback,
            skills_generation: 0,
            path: None,
            write_lock: Arc::new(ModelConfigurationLock::default()),
        })
    }

    /// Refresh the readable predefined copy, read only the explicit user layer, validate, then provision.
    pub fn load<F>(
        path: &Path,
        predefined_path: &Path,
        environment: &BTreeMap<String, String>,
        validator: F,
    ) -> Result<Self>
    where
        F: FnMut(&TemplateSpec) -> Result<()>,
    {
        if predefined_path.as_os_str().is_empty() {
            return Err(ConfigError::invalid_data(
                "predefined destination must not be empty".into(),
            ));
        }
        write_predefined(predefined_path)?;
        let content = read_user(path)?;
        let mut configuration = Self::from_content(&content, environment, validator)?;
        configuration.path = Some(path.to_owned());
        configuration.write_lock = path_lock(path)?;
        configuration.runtime.provision_directories()?;
        Ok(configuration)
    }

    /// Hold the file's shared write lock through a synchronous model-routing operation.
    ///
    /// The same thread can re-enter through a separate configuration handle for
    /// the same path. Other threads wait until the outermost operation completes.
    /// This method does not automatically roll back writes or flatten an operation's
    /// own `Result`; callbacks and explicit rollback belong inside the closure.
    pub fn model_configuration_transaction<T>(
        &mut self,
        operation: impl FnOnce(&mut ModelConfigurationTransaction<'_>) -> T,
    ) -> Result<T> {
        let lock = self.write_lock.clone();
        let _guard = lock.enter()?;
        Ok(operation(&mut ModelConfigurationTransaction {
            configuration: self,
            _thread_affine: PhantomData,
        }))
    }

    pub fn set_model(&mut self, model: &str) -> Result<()> {
        let lock = self.write_lock.clone();
        let _guard = lock.enter()?;
        let mut root = self.user_root()?;
        root.set("model", Node::scalar(model));
        self.write(&root)?;
        self.model = model.to_owned();
        Ok(())
    }

    pub fn set_model_alias(&mut self, name: &str, target: &str) -> Result<()> {
        self.model_configuration_transaction(|transaction| {
            transaction.set_model_alias(name, target)
        })?
    }

    fn set_model_alias_unlocked(&mut self, name: &str, target: &str) -> Result<()> {
        if !self.models.model_aliases.contains_key(name) {
            return Err(ConfigError::invalid_data(format!(
                "model alias \"{name}\" is not defined"
            )));
        }
        models::validate_alias_name(name)?;
        models::validate_model_selector(
            &format!("model_aliases.{name}.model_string"),
            target,
            true,
        )?;
        let mut root = self.user_root()?;
        ensure_mapping(ensure_mapping(&mut root, "model_aliases")?, name)?
            .set("model_string", Node::scalar(target));
        self.write(&root)?;
        self.models
            .model_aliases
            .get_mut(name)
            .expect("validated alias")
            .model_string = target.to_owned();
        Ok(())
    }

    pub fn set_model_aliases(&mut self, defaults: &ProviderModelAliasDefaults) -> Result<()> {
        self.model_configuration_transaction(|transaction| transaction.set_model_aliases(defaults))?
    }

    fn set_model_aliases_unlocked(&mut self, defaults: &ProviderModelAliasDefaults) -> Result<()> {
        let targets = defaults.targets();
        models::validate_provider_model_alias_defaults(&defaults.provider_id, &targets)?;
        self.set_routing_unlocked(None, &targets, None)
    }

    pub fn set_model_routing(
        &mut self,
        model: &str,
        targets: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.set_model_routing_with_limits(model, targets, None)
    }
    pub fn set_model_routing_with_limits(
        &mut self,
        model: &str,
        targets: &BTreeMap<String, String>,
        limits: Option<&ModelPresetContextLimits>,
    ) -> Result<()> {
        self.model_configuration_transaction(|transaction| {
            transaction.set_model_routing_with_limits(model, targets, limits)
        })?
    }

    fn set_model_routing_with_limits_unlocked(
        &mut self,
        model: &str,
        targets: &BTreeMap<String, String>,
        limits: Option<&ModelPresetContextLimits>,
    ) -> Result<()> {
        models::validate_requested_selector("model", model)?;
        self.set_routing_unlocked(Some(model), targets, limits)
    }
    fn set_routing_unlocked(
        &mut self,
        model: Option<&str>,
        targets: &BTreeMap<String, String>,
        limits: Option<&ModelPresetContextLimits>,
    ) -> Result<()> {
        let mut aliases = self.models.model_aliases.clone();
        let mut root = self.user_root()?;
        let alias_fields = ensure_mapping(&mut root, "model_aliases")?;
        for (name, target) in targets {
            let alias = aliases.get_mut(name).ok_or_else(|| {
                ConfigError::invalid_data(format!("model alias \"{name}\" is not defined"))
            })?;
            models::validate_model_selector(
                &format!("model_aliases.{name}.model_string"),
                target,
                true,
            )?;
            ensure_mapping(alias_fields, name)?.set("model_string", Node::scalar(target));
            alias.model_string = target.clone();
        }
        if let Some(limits) = limits {
            models::apply_context_limits(&mut root, &mut aliases, limits)?;
        }
        if let Some(model) = model {
            root.set("model", Node::scalar(model));
        }
        self.write(&root)?;
        if let Some(model) = model {
            self.model = model.to_owned();
        }
        if let Some(limits) = limits {
            self.models.context_limit = limits.default.clone();
        }
        self.models.model_aliases = aliases;
        Ok(())
    }

    pub fn set_model_preset(&mut self, name: &str, preset: &ModelPresetConfig) -> Result<()> {
        self.model_configuration_transaction(|transaction| {
            transaction.set_model_preset(name, preset)
        })?
    }

    fn set_model_preset_unlocked(&mut self, name: &str, preset: &ModelPresetConfig) -> Result<()> {
        models::validate_preset_name(name)?;
        models::validate_requested_selector(&format!("model_presets.{name}.model"), &preset.model)?;
        let mut aliases = Node::mapping(Vec::new());
        for (alias, target) in &preset.model_aliases {
            models::validate_alias_name(alias)?;
            models::validate_model_selector(
                &format!("model_presets.{name}.model_aliases.{alias}"),
                target,
                true,
            )?;
            aliases.set(alias, Node::scalar(target));
        }
        let mut root = self.user_root()?;
        let mut fields = Node::mapping(vec![
            (Node::scalar("model"), Node::scalar(&preset.model)),
            (Node::scalar("model_aliases"), aliases),
        ]);
        if let Some(limits) = &preset.context_limits {
            fields.set("context_limits", models::write_context_limits(limits));
        }
        ensure_mapping(&mut root, "model_presets")?.set(name, fields);
        self.write(&root)?;
        self.models
            .model_presets
            .insert(name.to_owned(), preset.clone());
        Ok(())
    }
    pub fn capture_context_limits(&self) -> ModelPresetContextLimits {
        ModelPresetContextLimits {
            default: self.models.context_limit.clone(),
            model_aliases: self
                .models
                .model_aliases
                .iter()
                .filter_map(|(name, alias)| {
                    alias
                        .context_limit
                        .clone()
                        .map(|limit| (name.clone(), limit))
                })
                .collect(),
        }
    }
    pub fn set_context_limit(&mut self, limit: ContextSize) -> Result<()> {
        self.model_configuration_transaction(|transaction| transaction.set_context_limit(limit))?
    }

    fn set_context_limit_unlocked(&mut self, limit: ContextSize) -> Result<()> {
        let mut root = self.user_root()?;
        root.set("context_limit", Node::scalar(limit.to_string()));
        self.write(&root)?;
        self.models.context_limit = Some(limit);
        Ok(())
    }
    pub fn set_skill_enabled(&mut self, path: &str, enabled: bool) -> Result<()> {
        let canonical = runtime::canonical_skill_path(path, "skills.entries.path")?;
        let lock = self.write_lock.clone();
        let _guard = lock.enter()?;
        let mut root = self.user_root()?;
        let skills = ensure_mapping(&mut root, "skills")?;
        if skills.get("entries").is_none() {
            skills.set("entries", Node::sequence(Vec::new()));
        }
        let entries = skills
            .get_mut("entries")
            .and_then(Node::as_sequence_mut)
            .ok_or_else(|| ConfigError::invalid_data("skills.entries must be a sequence".into()))?;
        entries.retain(|entry| {
            entry
                .get("path")
                .and_then(Node::as_str)
                .and_then(|value| runtime::canonical_skill_path(value, "skills.entries.path").ok())
                .as_deref()
                != Some(&canonical)
        });
        entries.push(Node::mapping(vec![
            (Node::scalar("path"), Node::scalar(canonical)),
            (
                Node::scalar("enabled"),
                Node::scalar(if enabled { "true" } else { "false" }),
            ),
        ]));
        let content = yaml::serialize(&root);
        let checked = yaml::parse_root(&content)?;
        let validated = runtime::read_skills(&checked)?;
        self.write_serialized(&content, true)?;
        self.runtime.skills = validated;
        self.skills_generation += 1;
        Ok(())
    }

    pub fn reload_model_configuration(&self) -> Result<ModelConfigurationSnapshot> {
        let mut root = yaml::parse_root(&predefined_content()?)?;
        let user = self.user_root()?;
        yaml::validate_replace_tags(&user)?;
        yaml::merge(&mut root, &user)?;
        let context_limit = models::read_context_limit(&root, "context_limit", "context_limit")?;
        let model = scalar_or_empty(&root, "model");
        let (sandbox_enabled, sandbox_dev_bind) = runtime::read_sandbox(&root)?;
        Ok(ModelConfigurationSnapshot {
            model,
            context_limit,
            sandbox_enabled,
            sandbox_dev_bind,
            model_aliases: models::read_model_aliases(&root)?,
            model_presets: models::read_model_presets(&root)?,
        })
    }

    pub fn model_configuration(&self) -> ModelConfigurationSnapshot {
        ModelConfigurationSnapshot {
            model: self.model.clone(),
            context_limit: self.models.context_limit.clone(),
            sandbox_enabled: self.runtime.sandbox_enabled,
            sandbox_dev_bind: self.runtime.sandbox_dev_bind.to_vec(),
            model_aliases: self.models.model_aliases.clone(),
            model_presets: self.models.model_presets.clone(),
        }
    }

    pub fn synchronize_model_configuration(&mut self, refreshed: &ModelConfigurationSnapshot) {
        self.model = refreshed.model.clone();
        self.models.context_limit = refreshed.context_limit.clone();
        self.models.model_aliases = refreshed.model_aliases.clone();
        self.models.model_presets = refreshed.model_presets.clone();
    }

    pub fn snapshot(&self, selectors: &[String]) -> serde_json::Value {
        use serde_json::json;
        let mut value = self.models.snapshot();
        let object = value.as_object_mut().expect("settings snapshot object");
        object.remove("modelProfiles");
        let resolutions:serde_json::Map<String,serde_json::Value>=selectors.iter().map(|selector| {
            let resolved=self.models.model_profiles.resolve(selector);
            (selector.clone(),json!({"usage":resolved.usage,"systemPrompt":resolved.system_prompt,"maximumInputTokens":resolved.maximum_input_tokens,"summaryOutputTokens":resolved.summary_output_tokens}))
        }).collect();
        object.insert("modelProfileResolutions".into(), json!(resolutions));
        object.insert("model".into(), json!(self.model));
        object.insert("systemPrompts".into(), json!(self.system_prompts));
        object.extend(
            self.runtime
                .snapshot()
                .as_object()
                .expect("runtime snapshot object")
                .clone(),
        );
        value
    }

    fn user_root(&self) -> Result<Node> {
        yaml::parse_root(&read_user(self.path.as_deref().ok_or_else(|| {
            ConfigError::invalid_data("configuration was not loaded from a file".into())
        })?)?)
    }
    fn write(&self, root: &Node) -> Result<()> {
        self.write_serialized(&yaml::serialize(root), false)
    }
    fn write_serialized(&self, content: &str, validate: bool) -> Result<()> {
        let path = self.path.as_deref().ok_or_else(|| {
            ConfigError::invalid_data("configuration was not loaded from a file".into())
        })?;
        create_parent(path)?;
        let temporary = PathBuf::from(format!("{}.tmp", path.display()));
        let result = (|| {
            std::fs::write(&temporary, content)?;
            if validate {
                let _ = yaml::parse_root(&std::fs::read_to_string(&temporary)?)?;
            }
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if validate {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// Model configuration operations admitted under one shared path write lock.
///
/// The token is synchronous and thread-affine. It cannot escape the transaction
/// closure, and it does not expose the configuration's private path or lock.
///
/// ```compile_fail
/// use parrot_foundation::config::configuration::ModelConfigurationTransaction;
/// fn send_transaction(transaction: &mut ModelConfigurationTransaction<'_>) {
///     std::thread::scope(|scope| {
///         scope.spawn(move || { let _ = transaction.snapshot(); });
///     });
/// }
/// ```
pub struct ModelConfigurationTransaction<'a> {
    configuration: &'a mut Configuration,
    _thread_affine: PhantomData<Rc<()>>,
}

impl ModelConfigurationTransaction<'_> {
    pub fn snapshot(&self) -> ModelConfigurationSnapshot {
        self.configuration.model_configuration()
    }

    pub fn reload(&self) -> Result<ModelConfigurationSnapshot> {
        self.configuration.reload_model_configuration()
    }

    pub fn synchronize(&mut self, refreshed: &ModelConfigurationSnapshot) {
        self.configuration
            .synchronize_model_configuration(refreshed);
    }

    pub fn set_model_alias(&mut self, name: &str, target: &str) -> Result<()> {
        self.configuration.set_model_alias_unlocked(name, target)
    }

    pub fn set_model_aliases(&mut self, defaults: &ProviderModelAliasDefaults) -> Result<()> {
        self.configuration.set_model_aliases_unlocked(defaults)
    }

    pub fn set_model_preset(&mut self, name: &str, preset: &ModelPresetConfig) -> Result<()> {
        self.configuration.set_model_preset_unlocked(name, preset)
    }

    pub fn set_model_routing_with_limits(
        &mut self,
        model: &str,
        targets: &BTreeMap<String, String>,
        limits: Option<&ModelPresetContextLimits>,
    ) -> Result<()> {
        self.configuration
            .set_model_routing_with_limits_unlocked(model, targets, limits)
    }

    pub fn set_context_limit(&mut self, limit: ContextSize) -> Result<()> {
        self.configuration.set_context_limit_unlocked(limit)
    }
}

fn ensure_mapping<'a>(parent: &'a mut Node, key: &str) -> Result<&'a mut Node> {
    if parent.get(key).is_none() {
        parent.set(key, Node::mapping(Vec::new()));
    }
    if !matches!(parent.get(key).map(|v| &v.kind), Some(Kind::Mapping(_))) {
        return Err(ConfigError::invalid_data(format!(
            "{key} must be a mapping"
        )));
    }
    Ok(parent.get_mut(key).expect("inserted mapping"))
}
fn read_user(path: &Path) -> Result<String> {
    // File.Exists is false for a missing path or directory; it is not a legacy-root probe.
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(std::fs::read_to_string(path)?),
        Ok(_) => Ok(String::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}
fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
fn write_predefined(path: &Path) -> Result<()> {
    let write = || -> Result<()> {
        create_parent(path)?;
        let temporary = path.with_file_name(format!(
            ".parrot-predefined-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&temporary, predefined_content()?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    };
    write()
        .map_err(|_| ConfigError::invalid_data("unable to write predefined configuration".into()))
}
static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[derive(Debug, Default)]
struct ModelConfigurationLock {
    state: Mutex<Admission>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct Admission {
    owner: Option<ThreadId>,
    depth: usize,
}

struct ModelConfigurationGuard<'a> {
    gate: &'a ModelConfigurationLock,
    _thread_affine: PhantomData<Rc<()>>,
}

impl ModelConfigurationLock {
    fn enter(&self) -> Result<ModelConfigurationGuard<'_>> {
        let current = std::thread::current().id();
        let mut state = self.state.lock().map_err(poison)?;
        while state.owner.is_some_and(|owner| owner != current) {
            state = self.available.wait(state).map_err(poison)?;
        }
        state.owner = Some(current);
        state.depth += 1;
        // Admission protects the operation, not this internal state mutex.
        drop(state);
        Ok(ModelConfigurationGuard {
            gate: self,
            _thread_affine: PhantomData,
        })
    }
}

impl Drop for ModelConfigurationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.depth -= 1;
        if state.depth == 0 {
            state.owner = None;
            self.gate.available.notify_one();
        }
    }
}

fn path_lock(path: &Path) -> Result<Arc<ModelConfigurationLock>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<ModelConfigurationLock>>>> = OnceLock::new();
    let cwd = std::env::current_dir()?;
    // The source lock key uses Path.GetFullPath, not macOS alias normalization.
    let platform = PathPlatform::Unix;
    let key = PlatformPath::normalize(&path.to_string_lossy(), &cwd.to_string_lossy(), platform)
        .map_err(|error| ConfigError::invalid_data(error.to_string()))?;
    Ok(LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(poison)?
        .entry(key)
        .or_insert_with(|| Arc::new(ModelConfigurationLock::default()))
        .clone())
}
fn poison<T>(_: std::sync::PoisonError<T>) -> ConfigError {
    ConfigError::invalid_data("configuration write lock is poisoned".into())
}
