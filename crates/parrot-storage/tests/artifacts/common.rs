use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t20-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    pub fn root(&self) -> SelectedSessionRoot {
        SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
pub struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[derive(Clone, Copy)]
pub struct Cancelled;
impl Cancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub struct Names(std::sync::Mutex<std::collections::VecDeque<String>>);
impl Names {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(std::sync::Mutex::new(
            names.into_iter().map(Into::into).collect(),
        ))
    }
}
impl parrot_domain::contracts::filenames::FilenameGenerator for Names {
    fn next_filename(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("deterministic name provider exhausted")
    }
}
