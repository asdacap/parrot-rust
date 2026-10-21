use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatePaths {
    state: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl StatePaths {
    pub fn new(state: PathBuf, config: PathBuf, data: PathBuf) -> Self {
        Self {
            state,
            config,
            data,
        }
    }

    pub fn resolve(environment: &HashMap<String, String>) -> Self {
        Self::for_application(environment, "parrotrust")
    }

    pub fn resolve_from_environment() -> Self {
        let environment = ["HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]
            .into_iter()
            .filter_map(|name| {
                std::env::var_os(name)
                    .map(|value| (name.to_owned(), value.to_string_lossy().into_owned()))
            })
            .collect();
        Self::resolve(&environment)
    }

    pub(super) fn for_application(
        environment: &HashMap<String, String>,
        application: &str,
    ) -> Self {
        let home = environment.get("HOME").map(String::as_str).unwrap_or("");
        let resolve = |name: &str, suffix: &[&str]| {
            let preferred = environment.get(name).map(String::as_str).unwrap_or("");
            let mut root = PathBuf::from(if preferred.is_empty() {
                home
            } else {
                preferred
            });
            if preferred.is_empty() {
                for part in suffix {
                    root.push(part);
                }
            }
            root.join(application)
        };
        Self::new(
            resolve("XDG_STATE_HOME", &[".local", "state"]),
            resolve("XDG_CONFIG_HOME", &[".config"]),
            resolve("XDG_DATA_HOME", &[".local", "share"]),
        )
    }

    pub fn state(&self) -> &Path {
        &self.state
    }
    pub fn config(&self) -> &Path {
        &self.config
    }
    pub fn data(&self) -> &Path {
        &self.data
    }
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yaml")
    }
    pub fn predefined_config_file(&self) -> PathBuf {
        self.config.join("predefined_config.yaml")
    }
    pub fn credentials_file(&self) -> PathBuf {
        self.config.join("credentials.json")
    }
    pub fn credential_preset_directory(&self) -> PathBuf {
        self.config.join("credential_presets")
    }
    pub fn skills_directory(&self) -> PathBuf {
        self.data.join("skills")
    }
    pub fn control(&self) -> PathBuf {
        self.state.join("control")
    }
    pub fn log_directory(&self) -> PathBuf {
        self.state.join("logs")
    }
}
