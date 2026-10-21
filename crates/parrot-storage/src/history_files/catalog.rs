use super::SessionMeta;
use parrot_domain::values::identifiers::UserSessionId;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCatalogState {
    Inactive,
    Corrupt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCatalogEntry {
    pub id: UserSessionId,
    pub state: SessionCatalogState,
    pub working_directory: String,
    pub root_agent_name: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub selector: Option<String>,
    pub mode: Option<String>,
    pub last_opened_at: Option<String>,
    pub created_at: Option<String>,
}

impl SessionCatalogEntry {
    fn corrupt(id: UserSessionId) -> Self {
        Self {
            id,
            state: SessionCatalogState::Corrupt,
            working_directory: String::new(),
            root_agent_name: Some(String::new()),
            provider_id: String::new(),
            model: String::new(),
            selector: Some(String::new()),
            mode: Some(String::new()),
            last_opened_at: Some(String::new()),
            created_at: Some(String::new()),
        }
    }
}

/// A metadata-only list of sessions beneath one already-selected state directory.
#[derive(Clone, Debug)]
pub struct SessionCatalog {
    sessions_directory: PathBuf,
}

impl SessionCatalog {
    pub fn new(selected_state_directory: impl Into<PathBuf>) -> Self {
        Self {
            sessions_directory: selected_state_directory.into().join("sessions"),
        }
    }

    pub fn sessions_directory(&self) -> &Path {
        &self.sessions_directory
    }

    pub fn find(&self, id: &UserSessionId) -> Option<SessionCatalogEntry> {
        let directory = self.sessions_directory.join(id.value());
        if !directory.is_dir() {
            return None;
        }
        Some(read_entry(&directory, id.clone()))
    }

    /// Invalid directory names are omitted; each valid but corrupt session is isolated.
    pub fn list(&self) -> Vec<SessionCatalogEntry> {
        let entries = match fs::read_dir(&self.sessions_directory) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut listed = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let directory = entry.path();
            if !directory.is_dir() {
                continue;
            }
            let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(id) = UserSessionId::try_parse(name) else {
                continue;
            };
            listed.push(read_entry(&directory, id));
        }
        listed
    }
}

fn read_entry(directory: &Path, expected_id: UserSessionId) -> SessionCatalogEntry {
    if is_symlink(directory) {
        return SessionCatalogEntry::corrupt(expected_id);
    }
    let metadata_path = directory.join("meta.json");
    if is_symlink(&metadata_path) {
        return SessionCatalogEntry::corrupt(expected_id);
    }
    let contents = match fs::read_to_string(&metadata_path) {
        Ok(contents) => contents,
        Err(_) => return SessionCatalogEntry::corrupt(expected_id),
    };
    let metadata: SessionMeta = match serde_json::from_str(&contents) {
        Ok(metadata) => metadata,
        Err(_) => return SessionCatalogEntry::corrupt(expected_id),
    };
    let Some(id) = metadata.id.as_deref().and_then(UserSessionId::try_parse) else {
        return SessionCatalogEntry::corrupt(expected_id);
    };
    let Some(working_directory) = metadata
        .working_directory
        .as_deref()
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
    else {
        return SessionCatalogEntry::corrupt(expected_id);
    };
    let Some(provider_id) = metadata
        .provider_id
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return SessionCatalogEntry::corrupt(expected_id);
    };
    let Some(model) = metadata.model.as_deref().filter(|value| !value.is_empty()) else {
        return SessionCatalogEntry::corrupt(expected_id);
    };
    if id != expected_id {
        return SessionCatalogEntry::corrupt(expected_id);
    }
    SessionCatalogEntry {
        id,
        state: SessionCatalogState::Inactive,
        working_directory: working_directory.to_owned(),
        root_agent_name: metadata.root_agent_name,
        provider_id: provider_id.to_owned(),
        model: model.to_owned(),
        selector: metadata.selector,
        mode: metadata.mode,
        last_opened_at: metadata.last_opened_at,
        created_at: metadata.created_at,
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}
