use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::values::identifiers::UserSessionId;
use parrot_foundation::paths::UserSessionResources;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

mod identity_safety;
mod identity_storage;
mod selected_roots;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);
impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "t21-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn l329_actual_session_resources_agent_scratch_blob_directory() {
    let directory = TempRoot::new("literal329");
    let id = UserSessionId::parse("user-session-listed").unwrap();
    let selected = SelectedSessionRoot::new(
        directory.0.join("sessions").join(id.value()),
        RootOrigin::Current,
    )
    .unwrap();
    let resources = UserSessionResources::new(selected, id, ()).unwrap();
    // The value under test comes from the real owning Foundation APIs.
    let scratch = resources.agent_scratch(&["main"]).unwrap();
    assert_eq!(
        scratch.blob_directory(),
        resources
            .root()
            .join("root-agents")
            .join("main")
            .join("blobs")
    );
    assert!(scratch.root().is_dir());
    assert!(scratch.scratch_path().is_dir());
    assert!(scratch.blob_directory().is_dir());
    println!(
        "T21_ASSERTION Parrot.Infrastructure.Tests::Parrot.Core.Tests.StorageInvariantTests.1.1.Listing_reads_meta_json_and_never_a_database.1.1.0::assertion.line329"
    );
}
