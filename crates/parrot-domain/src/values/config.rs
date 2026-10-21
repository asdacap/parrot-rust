use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelProfileConfig {
    pub usage: Option<String>,
    pub system_prompt: Option<String>,
    pub maximum_input_tokens: Option<i32>,
    pub summary_output_tokens: Option<i32>,
}

impl ModelProfileConfig {
    pub fn new(
        usage: Option<String>,
        system_prompt: Option<String>,
        maximum_input_tokens: Option<i32>,
        summary_output_tokens: Option<i32>,
    ) -> Self {
        Self {
            usage,
            system_prompt,
            maximum_input_tokens,
            summary_output_tokens,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelProfiles {
    profiles: BTreeMap<String, ModelProfileConfig>,
}

impl ModelProfiles {
    pub fn new(profiles: &BTreeMap<String, ModelProfileConfig>) -> Self {
        Self {
            profiles: profiles.clone(),
        }
    }

    pub fn resolve(&self, canonical_selector: &str) -> ModelProfileConfig {
        let mut resolved = ModelProfileConfig::default();
        let mut prefix = canonical_selector;

        while !prefix.is_empty() {
            if let Some(profile) = self.profiles.get(prefix) {
                if resolved.usage.is_none() {
                    resolved.usage.clone_from(&profile.usage);
                }
                if resolved.system_prompt.is_none() {
                    resolved.system_prompt.clone_from(&profile.system_prompt);
                }
                if resolved.maximum_input_tokens.is_none() {
                    resolved.maximum_input_tokens = profile.maximum_input_tokens;
                }
                if resolved.summary_output_tokens.is_none() {
                    resolved.summary_output_tokens = profile.summary_output_tokens;
                }
                if resolved.usage.is_some()
                    && resolved.system_prompt.is_some()
                    && resolved.maximum_input_tokens.is_some()
                    && resolved.summary_output_tokens.is_some()
                {
                    break;
                }
            }

            let Some(boundary) = prefix.rfind('/') else {
                break;
            };
            prefix = &prefix[..boundary];
        }

        resolved
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillConfigEntry {
    pub path: String,
    pub enabled: bool,
}

impl SkillConfigEntry {
    pub fn new(path: impl Into<String>, enabled: bool) -> Self {
        Self {
            path: path.into(),
            enabled,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillConfiguration {
    pub enabled: bool,
    pub entries: Vec<SkillConfigEntry>,
}

impl SkillConfiguration {
    pub fn new(enabled: bool, entries: Vec<SkillConfigEntry>) -> Self {
        Self { enabled, entries }
    }

    pub fn is_enabled(
        &self,
        canonical_path: &str,
        current_directory: &str,
        platform: PathPlatform,
    ) -> Result<bool, SkillConfigurationError> {
        if canonical_path.trim().is_empty() {
            return Err(SkillConfigurationError::BlankPath);
        }
        if !self.enabled {
            return Ok(false);
        }

        let normalized = PlatformPath::normalize(canonical_path, current_directory, platform)?;
        let mut result = None;
        for entry in &self.entries {
            if PlatformPath::normalize(&entry.path, current_directory, platform)? == normalized {
                result = Some(entry.enabled);
            }
        }
        Ok(result.unwrap_or(true))
    }
}

impl Default for SkillConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathPlatform {
    Unix,
    MacOs,
}

pub struct PlatformPath;

impl PlatformPath {
    pub fn normalize(
        path: &str,
        current_directory: &str,
        platform: PathPlatform,
    ) -> Result<String, PathNormalizationError> {
        if path.is_empty() {
            return Err(PathNormalizationError::EmptyPath);
        }
        if path.contains('\0') {
            return Err(PathNormalizationError::NulByte);
        }

        let absolute = if path.starts_with('/') {
            path.to_owned()
        } else {
            if !current_directory.starts_with('/') {
                return Err(PathNormalizationError::BaseMustBeAbsolute);
            }
            format!("{}/{}", current_directory.trim_end_matches('/'), path)
        };
        let mut segments = Vec::new();
        for segment in absolute.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                segment => segments.push(segment),
            }
        }
        let mut normalized = format!("/{}", segments.join("/"));
        if path.ends_with('/') && normalized != "/" {
            normalized.push('/');
        }
        if platform == PathPlatform::MacOs {
            normalized = match normalized.as_str() {
                "/var" => "/private/var".to_owned(),
                "/tmp" => "/private/tmp".to_owned(),
                "/etc" => "/private/etc".to_owned(),
                _ if ["/var/", "/tmp/", "/etc/"]
                    .iter()
                    .any(|prefix| normalized.starts_with(prefix)) =>
                {
                    format!("/private{normalized}")
                }
                _ => normalized,
            };
        }
        Ok(normalized)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathNormalizationError {
    EmptyPath,
    NulByte,
    BaseMustBeAbsolute,
}

impl fmt::Display for PathNormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => formatter.write_str("path is empty"),
            Self::NulByte => formatter.write_str("path contains a NUL byte"),
            Self::BaseMustBeAbsolute => formatter.write_str("base directory must be absolute"),
        }
    }
}

impl std::error::Error for PathNormalizationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillConfigurationError {
    BlankPath,
    Path(PathNormalizationError),
}

impl From<PathNormalizationError> for SkillConfigurationError {
    fn from(error: PathNormalizationError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for SkillConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankPath => formatter.write_str("canonical skill path cannot be blank"),
            Self::Path(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SkillConfigurationError {}
