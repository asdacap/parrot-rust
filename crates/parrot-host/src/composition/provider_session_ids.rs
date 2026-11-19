//! File persistence for provider session ids under the user session root.
use parrot_provider::sessions::{ProviderSessionId, ProviderSessionIds};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// One user session's persisted provider session ids, keyed by agent session and provider.
/// A missing, unreadable, or unparseable file reads as an empty store; a failed write keeps
/// the in-memory state and reports through the injected reporter.
pub struct FileProviderSessionIds {
    path: PathBuf,
    report: Arc<dyn Fn(&str) + Send + Sync>,
    writes: Mutex<()>,
}

#[derive(Serialize, Deserialize, Default)]
struct Persisted {
    #[serde(default)]
    agent_sessions: HashMap<String, HashMap<String, String>>,
}

impl FileProviderSessionIds {
    /// Opens the store that persists ids into one JSON file; write failures go to the reporter.
    pub fn open(path: PathBuf, report: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self {
            path,
            report,
            writes: Mutex::new(()),
        }
    }

    fn read(&self) -> Persisted {
        let Ok(contents) = fs::read_to_string(&self.path) else {
            return Persisted::default();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }
}

impl ProviderSessionIds for FileProviderSessionIds {
    fn stored(&self, agent_session_id: &str, provider_id: &str) -> Option<ProviderSessionId> {
        let persisted = self.read();
        let value = persisted
            .agent_sessions
            .get(agent_session_id)?
            .get(provider_id)?;
        ProviderSessionId::try_parse(value)
    }
    fn record(&self, agent_session_id: &str, provider_id: &str, session_id: &ProviderSessionId) {
        let _guard = self.writes.lock().unwrap();
        let mut persisted = self.read();
        persisted
            .agent_sessions
            .entry(agent_session_id.to_owned())
            .or_default()
            .insert(provider_id.to_owned(), session_id.value().to_owned());
        if let Err(error) = write_atomic(&self.path, &persisted) {
            (self.report)(&error.to_string());
        }
    }
}

fn write_atomic(path: &Path, persisted: &Persisted) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("provider session id file has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let body = serde_json::to_string_pretty(persisted)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, body)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "parrot-host-provider-session-ids-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("provider_sessions.json")
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open_store(path: PathBuf) -> FileProviderSessionIds {
        FileProviderSessionIds::open(
            path,
            Arc::new(|failure| {
                panic!("unexpected write failure: {failure}");
            }),
        )
    }

    fn id(hex: &str) -> ProviderSessionId {
        ProviderSessionId::try_parse(hex).unwrap()
    }

    #[test]
    fn round_trip_survives_a_new_store_instance() {
        let directory = TempDirectory::new();
        let first = open_store(directory.file());
        first.record(
            "agent-1",
            "provider-a",
            &id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"),
        );
        let second = open_store(directory.file());
        assert_eq!(
            second.stored("agent-1", "provider-a"),
            Some(id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"))
        );
        assert_eq!(second.stored("agent-1", "provider-b"), None);
        assert_eq!(second.stored("agent-2", "provider-a"), None);
    }

    #[test]
    fn writes_from_different_agent_sessions_keep_each_entry() {
        let directory = TempDirectory::new();
        let store = open_store(directory.file());
        store.record(
            "agent-1",
            "provider-a",
            &id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"),
        );
        store.record(
            "agent-2",
            "provider-a",
            &id("b0b1b2b3b4b5b6b7b8b9baabbcbdbebf"),
        );
        store.record(
            "agent-1",
            "provider-b",
            &id("c0c1c2c3c4c5c6c7c8c9caabcccdcecf"),
        );
        let reopened = open_store(directory.file());
        assert_eq!(
            reopened.stored("agent-1", "provider-a"),
            Some(id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"))
        );
        assert_eq!(
            reopened.stored("agent-2", "provider-a"),
            Some(id("b0b1b2b3b4b5b6b7b8b9baabbcbdbebf"))
        );
        assert_eq!(
            reopened.stored("agent-1", "provider-b"),
            Some(id("c0c1c2c3c4c5c6c7c8c9caabcccdcecf"))
        );
    }

    #[test]
    fn a_corrupt_file_reads_as_an_empty_store() {
        let directory = TempDirectory::new();
        fs::write(directory.file(), "not json {").unwrap();
        let reopened = open_store(directory.file());
        assert_eq!(reopened.stored("agent-1", "provider-a"), None);
        reopened.record(
            "agent-1",
            "provider-a",
            &id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"),
        );
        assert_eq!(
            open_store(directory.file()).stored("agent-1", "provider-a"),
            Some(id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"))
        );
    }

    #[test]
    fn an_invalid_stored_id_is_rejected() {
        let directory = TempDirectory::new();
        fs::write(
            directory.file(),
            r#"{"agent_sessions":{"agent-1":{"provider-a":"ZZZZ"}}}"#,
        )
        .unwrap();
        let reopened = open_store(directory.file());
        assert_eq!(reopened.stored("agent-1", "provider-a"), None);
    }

    #[test]
    fn a_failed_write_reports_and_keeps_the_memory_state() {
        let directory = TempDirectory::new();
        let failures = Arc::new(Mutex::new(Vec::new()));
        let path = directory
            .0
            .join("missing-dir")
            .join("provider_sessions.json");
        let store = FileProviderSessionIds::open(path.clone(), {
            let failures = failures.clone();
            Arc::new(move |failure| failures.lock().unwrap().push(failure.to_owned()))
        });
        std::fs::write(directory.0.join("missing-dir"), "not a directory").unwrap();
        store.record(
            "agent-1",
            "provider-a",
            &id("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"),
        );
        assert_eq!(failures.lock().unwrap().len(), 1);
    }
}
