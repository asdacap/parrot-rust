//! Source comparison: CompactionGroupBlobStoreTests.cs (5f27a8a9), plus
//! explicit basename/collision-budget/cancellation/identity regressions.
#![cfg(any(target_os = "linux", target_os = "macos"))]

// Compile the production implementation with cfg(test), keeping race-injection
// hooks internal rather than exposing them as part of the storage public API.
#[path = "../../src/artifacts/compaction.rs"]
mod implementation;

use implementation::{CompactionErrorKind, CompactionGroupBlobStore};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::filenames::FilenameGenerator;
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Names<F>(F);
impl<F: Fn() -> String + Send + Sync> FilenameGenerator for Names<F> {
    fn next_filename(&self) -> String {
        (self.0)()
    }
}

#[derive(Default)]
struct Token(AtomicBool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
impl Token {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Directory {
    container: PathBuf,
    root: SelectedSessionRoot,
    agent: PathBuf,
    blobs: PathBuf,
    moved: PathBuf,
    outside: PathBuf,
}
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let container = std::env::temp_dir().join(format!(
            "t20-compaction-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let session = container.join("selected-session");
        let agent = session.join("root-agents/main");
        let blobs = agent.join("blobs");
        let outside = container.join("outside");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = SelectedSessionRoot::new(session, RootOrigin::Current).unwrap();
        Self {
            moved: container.join("moved"),
            container,
            root,
            agent,
            blobs,
            outside,
        }
    }
    fn store(&self, names: &[&str], force_named: bool) -> CompactionGroupBlobStore {
        let names: Arc<Mutex<std::collections::VecDeque<String>>> = Arc::new(Mutex::new(
            names.iter().map(|name| (*name).to_owned()).collect(),
        ));
        let mut store = CompactionGroupBlobStore::for_session(
            &self.root,
            "root-agents/main",
            Arc::new(Names(move || {
                names
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected extra name attempt")
            })),
        )
        .unwrap();
        store.force_named_file = force_named;
        store
    }
    fn displaced_blobs(&self, ancestor: bool) -> PathBuf {
        if ancestor {
            self.moved.join("blobs")
        } else {
            self.moved.clone()
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.container);
    }
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
fn assert_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[test]
fn construction_is_lazy_on_unsupported_platform_and_cancellation_precedes_platform_failure() {
    let directory = Directory::new();
    fs::remove_dir(&directory.blobs).unwrap();
    let mut store = directory.store(&["unsupported.json"], false);
    store.unsupported_platform = true;
    let token = Token::default();
    let error = store.write_to_scratch(b"text", &token).unwrap_err();
    assert_eq!(error.kind, CompactionErrorKind::UnsupportedPlatform);
    assert!(!directory.blobs.exists());
    token.cancel();
    let error = store.write_to_scratch(b"text", &token).unwrap_err();
    assert_eq!(error.kind, CompactionErrorKind::Cancelled);
    assert_eq!(error.message, "The operation was canceled.");
}

#[test]
fn one_staging_file_preserves_eight_mib_and_retries_publish_collisions() {
    for named in [false, true] {
        let directory = Directory::new();
        fs::write(directory.blobs.join("existing.json"), "existing").unwrap();
        let mut store = directory.store(&["existing.json", "fallback.json"], named);
        let creations = Arc::new(AtomicUsize::new(0));
        let count = creations.clone();
        store.hooks.after_file_created = Some(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let text = "x".repeat(8 * 1024 * 1024);
        let path = store
            .write_to_scratch(text.as_bytes(), &Token::default())
            .unwrap();
        assert_eq!(path, directory.blobs.join("fallback.json"));
        assert_eq!(creations.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            entries(&directory.blobs),
            ["existing.json", "fallback.json"]
        );
        assert_private(&path);
    }
}

#[test]
fn scratch_write_preserves_raw_bytes_and_returns_the_published_path() {
    for named in [false, true] {
        let directory = Directory::new();
        let mut store = directory.store(&["scratch.json", "empty.json"], named);
        let contents = b"not JSON\0\xff\xfe\n";
        let path = store.write_to_scratch(contents, &Token::default()).unwrap();
        assert_eq!(path, directory.blobs.join("scratch.json"));
        assert_eq!(fs::read(&path).unwrap(), contents);
        let empty_path = store.write_to_scratch(&[], &Token::default()).unwrap();
        assert_eq!(empty_path, directory.blobs.join("empty.json"));
        assert!(fs::read(&empty_path).unwrap().is_empty());
        assert_eq!(entries(&directory.blobs), ["empty.json", "scratch.json"]);
    }
}

#[test]
fn cancelled_scratch_write_removes_staging_and_final_files() {
    for named in [false, true] {
        let directory = Directory::new();
        let token = Arc::new(Token::default());
        let cancel = token.clone();
        let mut store = directory.store(&["canceled-scratch.json"], named);
        store.hooks.after_file_created = Some(Box::new(move || cancel.cancel()));
        let error = store
            .write_to_scratch(&[0, 255, 1], token.as_ref())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::Cancelled);
        assert_eq!(error.message, "A task was canceled.");
        assert!(error.cleanup_failures.is_empty());
        assert!(entries(&directory.blobs).is_empty());
    }
}

#[test]
fn pre_cancelled_persist_creates_no_artifact() {
    for named in [false, true] {
        let directory = Directory::new();
        let mut store = directory.store(&["canceled.json"], named);
        let token = Token::default();
        token.cancel();
        let error = store.write_to_scratch(b"text", &token).unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::Cancelled);
        assert_eq!(error.message, "The operation was canceled.");
        assert!(entries(&directory.blobs).is_empty());
    }
}

#[test]
fn rejects_blob_and_ancestor_symlink_replacement_before_creation() {
    for ancestor in [false, true] {
        for named in [false, true] {
            let directory = Directory::new();
            let mut store = directory.store(&["escape.json"], named);
            let original = if ancestor {
                directory.agent.clone()
            } else {
                directory.blobs.clone()
            };
            let moved = directory.moved.clone();
            let outside = directory.outside.clone();
            fs::create_dir_all(directory.outside.join("blobs")).unwrap();
            store.hooks.after_blob_directory_opened = Some(Box::new(move || {
                fs::rename(&original, &moved).unwrap();
                std::os::unix::fs::symlink(&outside, &original).unwrap();
            }));
            let error = store
                .write_to_scratch(b"secret", &Token::default())
                .unwrap_err();
            assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
            assert!(entries(&directory.displaced_blobs(ancestor)).is_empty());
            assert!(!directory.outside.join("escape.json").exists());
            assert!(!directory.outside.join("blobs/escape.json").exists());
        }
    }
}

#[test]
fn removes_handle_relative_candidate_after_directory_replacement_during_creation() {
    for named in [false, true] {
        let directory = Directory::new();
        fs::write(directory.outside.join("escape.json"), "outside").unwrap();
        let mut store = directory.store(&["escape.json"], named);
        let original = directory.blobs.clone();
        let moved = directory.moved.clone();
        let outside = directory.outside.clone();
        store.hooks.after_file_created = Some(Box::new(move || {
            fs::rename(&original, &moved).unwrap();
            std::os::unix::fs::symlink(&outside, &original).unwrap();
        }));
        let error = store
            .write_to_scratch("x".repeat(100_000).as_bytes(), &Token::default())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
        assert!(error.cleanup_failures.is_empty());
        assert!(entries(&directory.moved).is_empty());
        assert_eq!(
            fs::read_to_string(directory.outside.join("escape.json")).unwrap(),
            "outside"
        );
    }
}

#[test]
fn rolls_back_final_link_after_directory_or_ancestor_replacement() {
    for named in [false, true] {
        for ancestor in [false, true] {
            let directory = Directory::new();
            let mut store = directory.store(&["post-link.json"], named);
            let original = if ancestor {
                directory.agent.clone()
            } else {
                directory.blobs.clone()
            };
            let moved = directory.moved.clone();
            let blobs = directory.blobs.clone();
            store.hooks.after_final_link_created = Some(Box::new(move || {
                fs::rename(&original, &moved).unwrap();
                fs::create_dir_all(&blobs).unwrap();
                fs::write(blobs.join("post-link.json"), "replacement").unwrap();
            }));
            let error = store
                .write_to_scratch(b"secret payload", &Token::default())
                .unwrap_err();
            assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
            assert!(error.cleanup_failures.is_empty());
            assert!(entries(&directory.displaced_blobs(ancestor)).is_empty());
            assert_eq!(
                fs::read_to_string(directory.blobs.join("post-link.json")).unwrap(),
                "replacement"
            );
        }
    }
}

#[test]
fn rollback_preserves_unrelated_final_link_replacement() {
    for named in [false, true] {
        let directory = Directory::new();
        let mut store = directory.store(&["replaced-link.json"], named);
        let blobs = directory.blobs.clone();
        let moved = directory.moved.clone();
        store.hooks.after_final_link_created = Some(Box::new(move || {
            let artifact = blobs.join("replaced-link.json");
            fs::remove_file(&artifact).unwrap();
            fs::write(&artifact, "unrelated").unwrap();
            fs::rename(&blobs, &moved).unwrap();
            fs::create_dir_all(&blobs).unwrap();
        }));
        let error = store
            .write_to_scratch(b"secret payload", &Token::default())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
        assert!(error.cleanup_failures.is_empty());
        assert_eq!(
            fs::read_to_string(directory.moved.join("replaced-link.json")).unwrap(),
            "unrelated"
        );
        assert_eq!(entries(&directory.moved), ["replaced-link.json"]);
        assert!(entries(&directory.blobs).is_empty());
    }
}

#[test]
fn rollback_quarantines_before_inspection_and_blocks_destination_recreation() {
    for named in [false, true] {
        let directory = Directory::new();
        let mut store = directory.store(&["cleanup-race.json"], named);
        let blobs = directory.blobs.clone();
        let moved = directory.moved.clone();
        store.hooks.after_final_link_created = Some(Box::new(move || {
            let artifact = blobs.join("cleanup-race.json");
            fs::remove_file(&artifact).unwrap();
            fs::write(&artifact, "unrelated").unwrap();
            fs::rename(&blobs, &moved).unwrap();
            fs::create_dir_all(&blobs).unwrap();
        }));
        let blocked = Arc::new(AtomicBool::new(false));
        let flag = blocked.clone();
        let moved = directory.moved.clone();
        store.hooks.after_cleanup_selection = Some(Box::new(move |name| {
            if name == "cleanup-race.json" {
                assert!(fs::metadata(moved.join(name)).unwrap().is_dir());
                assert!(fs::write(moved.join(name), "recreated").is_err());
                flag.store(true, Ordering::SeqCst);
            }
        }));
        let error = store
            .write_to_scratch(b"secret payload", &Token::default())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
        assert!(error.cleanup_failures.is_empty());
        assert!(blocked.load(Ordering::SeqCst));
        assert_eq!(
            fs::read_to_string(directory.moved.join("cleanup-race.json")).unwrap(),
            "unrelated"
        );
        assert_eq!(entries(&directory.moved), ["cleanup-race.json"]);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn statx_fails_closed_if_either_required_identity_field_is_missing() {
    for missing in [0x1, 0x100] {
        let directory = Directory::new();
        let mut store = directory.store(&["missing-inode.json"], true);
        store.hooks.returned_status_mask = Some(Box::new(move |mask| mask & !missing));
        let error = store
            .write_to_scratch(b"secret payload", &Token::default())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::Io);
        assert!(
            error
                .message
                .contains("required compaction group identity fields")
        );
        assert!(entries(&directory.blobs).is_empty());
    }
}

#[test]
fn artifacts_are_exactly_owner_read_write() {
    for named in [false, true] {
        let directory = Directory::new();
        let mut store = directory.store(&["private.json"], named);
        let path = store.write_to_scratch(b"text", &Token::default()).unwrap();
        assert_private(&path);
    }
}

#[test]
fn final_collisions_have_exactly_sixteen_exclusive_attempts_and_no_staging_leak() {
    for named in [false, true] {
        let directory = Directory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut store = CompactionGroupBlobStore::for_session(
            &directory.root,
            "root-agents/main",
            Arc::new(Names(move || {
                count.fetch_add(1, Ordering::SeqCst);
                "existing-arse.dat".to_owned()
            })),
        )
        .unwrap();
        store.force_named_file = named;
        fs::write(directory.blobs.join("existing.json"), "existing").unwrap();
        let error = store
            .write_to_scratch(b"secret", &Token::default())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::Io);
        assert_eq!(calls.load(Ordering::SeqCst), 16);
        assert!(error.cleanup_failures.is_empty());
        assert_eq!(entries(&directory.blobs), ["existing.json"]);
        assert_eq!(
            fs::read_to_string(directory.blobs.join("existing.json")).unwrap(),
            "existing"
        );
    }
}

#[test]
fn cancellation_between_publish_collisions_stops_before_next_name_and_cleans_staging() {
    for named in [false, true] {
        let directory = Directory::new();
        fs::write(directory.blobs.join("existing.json"), "existing").unwrap();
        let token = Arc::new(Token::default());
        let cancel = token.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut store = CompactionGroupBlobStore::for_session(
            &directory.root,
            "root-agents/main",
            Arc::new(Names(move || {
                count.fetch_add(1, Ordering::SeqCst);
                cancel.cancel();
                "existing-arse.dat".to_owned()
            })),
        )
        .unwrap();
        store.force_named_file = named;
        let error = store
            .write_to_scratch(b"secret", token.as_ref())
            .unwrap_err();
        assert_eq!(error.kind, CompactionErrorKind::Cancelled);
        assert!(error.cleanup_failures.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(entries(&directory.blobs), ["existing.json"]);
        assert_eq!(
            fs::read_to_string(directory.blobs.join("existing.json")).unwrap(),
            "existing"
        );
    }
}

#[test]
fn final_publish_does_not_overwrite_symlinks_or_directories() {
    for named in [false, true] {
        let directory = Directory::new();
        let target = directory.outside.join("target");
        fs::write(&target, "outside").unwrap();
        std::os::unix::fs::symlink(&target, directory.blobs.join("symlink.json")).unwrap();
        fs::create_dir(directory.blobs.join("directory.json")).unwrap();
        let mut store = directory.store(&["symlink.json", "directory.json", "final.json"], named);
        let path = store
            .write_to_scratch(b"secret", &Token::default())
            .unwrap();
        assert_eq!(path, directory.blobs.join("final.json"));
        assert!(
            fs::symlink_metadata(directory.blobs.join("symlink.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(directory.blobs.join("directory.json").is_dir());
        assert_eq!(fs::read_to_string(target).unwrap(), "outside");
        assert_eq!(
            entries(&directory.blobs),
            ["directory.json", "final.json", "symlink.json"]
        );
    }
}

#[test]
fn rejects_unsafe_basenames_and_replaces_only_terminal_generator_suffix() {
    for bad in [
        "../escape.json",
        "/escape.json",
        "a/b.json",
        "a\\b.json",
        ".json",
        "name.JSON",
        "",
        "-arse.dat",
        "../x-arse.dat",
    ] {
        for named in [false, true] {
            let directory = Directory::new();
            let mut store = directory.store(&[bad], named);
            let error = store
                .write_to_scratch(b"secret", &Token::default())
                .unwrap_err();
            assert_eq!(error.kind, CompactionErrorKind::InvalidOperation, "{bad:?}");
            assert!(error.cleanup_failures.is_empty());
            assert!(entries(&directory.blobs).is_empty());
        }
    }
    let directory = Directory::new();
    let mut store = directory.store(&["a-arse.dat-b-arse.dat"], true);
    let path = store.write_to_scratch(b"text", &Token::default()).unwrap();
    assert_eq!(path, directory.blobs.join("a-arse.dat-b.json"));
}

#[test]
fn selected_root_rejects_escaping_agent_resources_and_never_falls_back() {
    let directory = Directory::new();
    for bad in [
        "",
        "../other-agent",
        "/tmp/other-agent",
        "root-agents/../../other-agent",
    ] {
        assert!(
            CompactionGroupBlobStore::for_session(
                &directory.root,
                bad,
                Arc::new(Names(|| "x-arse.dat".to_owned()))
            )
            .is_err()
        );
    }
    fs::remove_dir(&directory.blobs).unwrap();
    let mut store = directory.store(&["missing.json"], true);
    let error = store
        .write_to_scratch(b"secret", &Token::default())
        .unwrap_err();
    assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
    assert!(entries(&directory.outside).is_empty());
}

#[test]
fn directory_identity_is_pinned_across_separate_persist_calls() {
    let directory = Directory::new();
    let mut store = directory.store(&["first.json", "second.json"], true);
    store.write_to_scratch(b"first", &Token::default()).unwrap();
    fs::rename(&directory.blobs, &directory.moved).unwrap();
    fs::create_dir(&directory.blobs).unwrap();
    let error = store
        .write_to_scratch(b"second", &Token::default())
        .unwrap_err();
    assert_eq!(error.kind, CompactionErrorKind::InvalidOperation);
    assert_eq!(entries(&directory.moved), ["first.json"]);
    assert!(entries(&directory.blobs).is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn cleanup_failures_are_attached_without_replacing_primary_failure() {
    let directory = Directory::new();
    let mut store = directory.store(&["cleanup-failure.json"], true);
    let published = Arc::new(AtomicBool::new(false));
    let flag = published.clone();
    store.hooks.after_final_link_created = Some(Box::new(move || {
        flag.store(true, Ordering::SeqCst);
    }));
    store.hooks.returned_status_mask = Some(Box::new(move |mask| {
        if published.load(Ordering::SeqCst) {
            mask & !0x100
        } else {
            mask
        }
    }));
    let error = store
        .write_to_scratch(b"secret", &Token::default())
        .unwrap_err();
    assert_eq!(error.kind, CompactionErrorKind::Io);
    assert!(
        error
            .message
            .contains("required compaction group identity fields")
    );
    assert_eq!(error.cleanup_failures.len(), 2);
    assert!(error.cleanup_failures.iter().all(|error| {
        error
            .message
            .contains("required compaction group identity fields")
    }));
    assert!(
        entries(&directory.blobs)
            .iter()
            .all(|name| !name.ends_with(".cleanup"))
    );
}
