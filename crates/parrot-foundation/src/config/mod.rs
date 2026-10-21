//! Source-compatible layered YAML configuration.

pub mod configuration;
pub mod environment;
pub mod models;
pub mod node;
pub mod opaque_json;
pub mod runtime;
pub mod templates;
pub mod yaml;
pub use configuration::Configuration;
pub use opaque_json::yaml_json;

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigErrorKind {
    InvalidData,
    Io,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ConfigError(pub String, ConfigErrorKind);
impl ConfigError {
    pub fn invalid_data(message: String) -> Self {
        Self(message, ConfigErrorKind::InvalidData)
    }

    pub fn kind(&self) -> ConfigErrorKind {
        self.1
    }
}
impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ConfigError").field(&self.0).finish()
    }
}
impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ConfigError {}
impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self(value.to_string(), ConfigErrorKind::Io)
    }
}
pub type Result<T> = std::result::Result<T, ConfigError>;
