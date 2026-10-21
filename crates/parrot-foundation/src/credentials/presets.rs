use super::model::CredentialError;
use super::store::CredentialStore;
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::fs;
use std::path::{Path, PathBuf};

pub struct CredentialPresets {
    directory: PathBuf,
}

impl CredentialPresets {
    /// Presets are rooted only in this explicitly selected configuration directory.
    pub fn at_selected_config_directory(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().join("credential_presets"),
        }
    }
    pub fn path(&self) -> &Path {
        &self.directory
    }

    pub fn list<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<String>, CredentialError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            let directory = self.directory.clone();
            let job = tokio::task::spawn_blocking(move || {
                let read = match fs::read_dir(&directory) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Vec::new());
                    }
                    Err(_) => return Err(CredentialError::Malformed),
                };
                let mut names = Vec::new();
                for entry in read {
                    let entry = entry.map_err(|_| CredentialError::Malformed)?;
                    if entry
                        .file_type()
                        .map_err(|_| CredentialError::Malformed)?
                        .is_file()
                    {
                        let path = entry.path();
                        if path.extension().is_some_and(|value| value == "json") {
                            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                                names.push(stem.to_owned());
                            }
                        }
                    }
                }
                names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                Ok(names)
            });
            match job.await {
                Ok(Ok(names)) => Ok(names),
                Ok(Err(error)) => Err(OperationError::Failed(error)),
                Err(_) => Err(OperationError::Failed(CredentialError::Malformed)),
            }
        })
    }

    pub fn save<'a>(
        &'a self,
        name: &'a str,
        current: &'a CredentialStore,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), CredentialError>> {
        Box::pin(async move {
            self.validate_name(name).map_err(OperationError::Failed)?;
            let snapshot = current.snapshot(cancellation).await?;
            self.store(name)
                .replace_snapshot(snapshot, cancellation)
                .await
        })
    }

    pub fn match_store<'a>(
        &'a self,
        current: &'a CredentialStore,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<String>, CredentialError>> {
        Box::pin(async move {
            let snapshot = current.snapshot(cancellation).await?;
            if snapshot.is_empty() {
                return Ok(None);
            }
            for name in self.list(cancellation).await? {
                let candidate = self.store(&name).snapshot(cancellation).await?;
                if candidate.len() == snapshot.len()
                    && snapshot.iter().all(|(key, value)| {
                        candidate.get(key).is_some_and(|other| value.matches(other))
                    })
                {
                    return Ok(Some(name));
                }
            }
            Ok(None)
        })
    }

    pub fn apply<'a>(
        &'a self,
        name: &'a str,
        current: &'a CredentialStore,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), CredentialError>> {
        Box::pin(async move {
            self.validate_name(name).map_err(OperationError::Failed)?;
            let path = self.path_for(name);
            let exists = tokio::task::spawn_blocking(move || path.is_file())
                .await
                .map_err(|_| OperationError::Failed(CredentialError::Malformed))?;
            if !exists {
                return Err(OperationError::Failed(CredentialError::NotFound));
            }
            let snapshot = self.store(name).snapshot(cancellation).await?;
            current.replace_snapshot(snapshot, cancellation).await
        })
    }

    fn validate_name(&self, name: &str) -> Result<(), CredentialError> {
        if name.is_empty()
            || name.trim() != name
            || name.contains('/')
            || (!cfg!(unix) && name.contains('\\'))
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(CredentialError::InvalidPresetName);
        }
        Ok(())
    }
    fn path_for(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.json"))
    }
    fn store(&self, name: &str) -> CredentialStore {
        CredentialStore::at_selected_path(self.path_for(name))
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Credential;
    use super::*;
    use parrot_domain::contracts::async_ops::BoxFuture;
    struct NeverCancelled;
    impl Cancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }
    static NEVER: NeverCancelled = NeverCancelled;
    struct AlreadyCancelled;
    impl Cancellation for AlreadyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    static CANCELLED: AlreadyCancelled = AlreadyCancelled;

    struct CancellationArrivesAfterCheck;
    impl Cancellation for CancellationArrivesAfterCheck {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    static CANCEL_AFTER_CHECK: CancellationArrivesAfterCheck = CancellationArrivesAfterCheck;

    struct CancelAfterEntryRemoved {
        store_path: PathBuf,
        entry: &'static str,
    }
    impl Cancellation for CancelAfterEntryRemoved {
        fn is_cancelled(&self) -> bool {
            fs::read_to_string(&self.store_path)
                .map(|contents| !contents.contains(self.entry))
                .unwrap_or(false)
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                if !self.is_cancelled() {
                    std::future::pending::<()>().await;
                }
            })
        }
    }

    fn value<T>(result: OperationResult<T, CredentialError>) -> Result<T, CredentialError> {
        result.map_err(|error| match error {
            OperationError::Cancelled => panic!("unexpected cancellation"),
            OperationError::Failed(error) => error,
        })
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "parrot-presets-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[tokio::test]
    async fn preset_snapshot_matches_applies_and_sorts() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let current = CredentialStore::at_selected_path(root.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&root);
        value(
            current
                .set("b", &Credential::for_api_key("k"), &NEVER)
                .await,
        )
        .unwrap();
        value(presets.save("acct", &current, &NEVER).await).unwrap();
        assert_eq!(value(presets.list(&NEVER).await).unwrap(), ["acct"]);
        assert_eq!(
            value(presets.match_store(&current, &NEVER).await).unwrap(),
            Some("acct".into())
        );
        value(
            current
                .set("extra", &Credential::for_api_key("other"), &NEVER)
                .await,
        )
        .unwrap();
        value(presets.apply("acct", &current, &NEVER).await).unwrap();
        assert_eq!(value(current.list(&NEVER).await).unwrap(), ["b"]);
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn literal_backslash_preset_round_trips_only_in_selected_new_root() {
        let root = root();
        let selected = root.join("new");
        let legacy = root.join("legacy").join("credential_presets");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("legacy.json"), b"unchanged legacy preset").unwrap();
        let current = CredentialStore::at_selected_path(selected.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&selected);
        let credential = Credential::for_api_key("saved-key");
        let name = r"work\account";
        value(current.set("provider", &credential, &NEVER).await).unwrap();

        value(presets.save(name, &current, &NEVER).await).unwrap();
        assert_eq!(presets.path(), selected.join("credential_presets"));
        assert!(presets.path().join(r"work\account.json").is_file());
        assert_eq!(value(presets.list(&NEVER).await).unwrap(), [name]);
        assert_eq!(
            value(presets.match_store(&current, &NEVER).await).unwrap(),
            Some(name.into())
        );
        value(
            current
                .set("provider", &Credential::for_api_key("changed-key"), &NEVER)
                .await,
        )
        .unwrap();
        value(presets.apply(name, &current, &NEVER).await).unwrap();
        let restored = value(current.get("provider", &NEVER).await)
            .unwrap()
            .unwrap();
        assert!(credential.matches(&restored));
        assert_eq!(
            fs::read(legacy.join("legacy.json")).unwrap(),
            b"unchanged legacy preset"
        );
        assert!(!legacy.join(r"work\account.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn preset_names_cannot_escape_selected_directory() {
        let root = root();
        let selected = root.join("new");
        let current = CredentialStore::at_selected_path(selected.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&selected);
        let absolute = root.join("outside").to_string_lossy().into_owned();
        for name in [
            "",
            " leading",
            "trailing ",
            "two words",
            "tab\tname",
            "line\nname",
            "nul\0name",
            "delete\u{7f}name",
            "work/account",
            "../outside",
            "work/../outside",
            absolute.as_str(),
        ] {
            assert_eq!(
                value(presets.save(name, &current, &NEVER).await).unwrap_err(),
                CredentialError::InvalidPresetName,
                "save accepted {name:?}"
            );
            assert_eq!(
                value(presets.apply(name, &current, &NEVER).await).unwrap_err(),
                CredentialError::InvalidPresetName,
                "apply accepted {name:?}"
            );
        }
        assert!(!root.exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn backslash_preset_names_remain_invalid_on_windows() {
        let root = root();
        let current = CredentialStore::at_selected_path(root.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&root);
        for name in [r"work\account", r"..\outside"] {
            assert_eq!(
                value(presets.save(name, &current, &NEVER).await).unwrap_err(),
                CredentialError::InvalidPresetName
            );
            assert_eq!(
                value(presets.apply(name, &current, &NEVER).await).unwrap_err(),
                CredentialError::InvalidPresetName
            );
        }
        assert!(!root.exists());
    }
    #[tokio::test]
    async fn list_ignores_cancellation_after_initial_check() {
        let root = root();
        let presets = CredentialPresets::at_selected_config_directory(&root);
        assert_eq!(
            value(presets.list(&CANCEL_AFTER_CHECK).await).unwrap(),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn apply_cancellation_preserves_completed_entry_progress() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let current = CredentialStore::at_selected_path(root.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&root);
        let preset = CredentialStore::at_selected_path(presets.path().join("desired.json"));
        value(
            current
                .set("obsolete", &Credential::for_api_key("old"), &NEVER)
                .await,
        )
        .unwrap();
        value(
            preset
                .set("wanted", &Credential::for_api_key("new"), &NEVER)
                .await,
        )
        .unwrap();

        let cancellation = CancelAfterEntryRemoved {
            store_path: current.path().to_path_buf(),
            entry: "obsolete",
        };
        assert!(matches!(
            presets.apply("desired", &current, &cancellation).await,
            Err(OperationError::Cancelled)
        ));
        assert_eq!(
            value(current.list(&NEVER).await).unwrap(),
            Vec::<String>::new()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn list_and_apply_preserve_cancellation_precedence() {
        let root = root();
        let current = CredentialStore::at_selected_path(root.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&root);
        assert!(matches!(
            presets.list(&CANCELLED).await,
            Err(OperationError::Cancelled)
        ));
        assert!(matches!(
            presets.apply("missing", &current, &CANCELLED).await,
            Err(OperationError::Failed(CredentialError::NotFound))
        ));
        assert!(matches!(
            presets.save("../outside", &current, &CANCELLED).await,
            Err(OperationError::Failed(CredentialError::InvalidPresetName))
        ));
    }

    #[tokio::test]
    async fn match_empty_store_is_none() {
        let root = root();
        let current = CredentialStore::at_selected_path(root.join("credentials.json"));
        let presets = CredentialPresets::at_selected_config_directory(&root);
        assert_eq!(
            value(presets.match_store(&current, &NEVER).await).unwrap(),
            None
        );
    }
}
