mod agent;
mod compaction;
mod forks;
mod reads;
mod reload_cancellation;
mod upstream_remaining;
mod upstream_statistics_history;

use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_storage::conversation::ConversationRepository;
use parrot_storage::database::SessionDatabase;
use parrot_storage::events::EventRepository;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-t18-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn repository(&self) -> ConversationRepository {
        let root = SelectedSessionRoot::new(self.0.clone(), RootOrigin::Current).unwrap();
        ConversationRepository::new(EventRepository::new(SessionDatabase::open(root).unwrap()))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
