//! Shared helpers for exact source-assertion translations; no template-engine substitute.
use parrot_foundation::config::{Configuration, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Context {
    pub directory: PathBuf,
    pub path: PathBuf,
    pub predefined: PathBuf,
    pub environment: BTreeMap<String, String>,
}
impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}
impl Context {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.tmp");
        std::fs::create_dir_all(&base).unwrap();
        let directory = loop {
            let candidate = base.join(format!(
                "t11-source-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create source assertion fixture: {error}"),
            }
        };
        let directory = std::fs::canonicalize(directory).unwrap();
        let path = directory.join("config.yaml");
        let predefined = directory.join("predefined.yaml");
        Self {
            directory,
            path,
            predefined,
            environment: super::fixtures::environment(),
        }
    }
    pub fn write(&self, yaml: &str) -> &Path {
        std::fs::write(&self.path, yaml).unwrap();
        &self.path
    }
    pub fn load(&self) -> Result<Configuration> {
        super::fixtures::fixture_load_with_environment(
            &self.path,
            &self.predefined,
            &self.environment,
        )
    }
    pub fn configured(&self, yaml: &str) -> Result<Configuration> {
        self.write(yaml);
        self.load()
    }
    pub fn saved(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }
}
