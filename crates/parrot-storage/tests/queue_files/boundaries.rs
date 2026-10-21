use super::{Directory, strings};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_storage::queue_files::{
    Direction, Error, ErrorKind, Inventory, MAXIMUM_FILE_BYTES, Owner, QueueState, QueueStore,
    Result,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};

#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    changed: tokio::sync::Notify,
}
impl Cancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_cancelled() {
                notified.await;
            }
        })
    }
}

fn owner() -> Owner {
    Owner {
        session_id: "agent-owner".into(),
        name: "main".into(),
        parent_session_id: String::new(),
        parent_name: String::new(),
    }
}

#[derive(Default)]
struct Recording {
    registered: Mutex<Vec<QueueState>>,
    updates: Mutex<Vec<QueueState>>,
    unregisters: Mutex<usize>,
}
impl Inventory for Recording {
    fn register_owner(&self, queues: &[QueueState]) -> Result<()> {
        *self.registered.lock().unwrap() = queues.to_vec();
        Ok(())
    }
    fn update(&self, state: QueueState) -> Result<()> {
        self.updates.lock().unwrap().push(state);
        Ok(())
    }
    fn unregister_owner(&self) -> Result<()> {
        *self.unregisters.lock().unwrap() += 1;
        Ok(())
    }
}

#[test]
fn exact_dotnet_created_files_read_and_rewrite_without_encoding_drift() {
    let directory = Directory::new();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/queue_files/fixtures/dotnet-initial");
    let store = directory.store();
    for name in [
        "ascii",
        "build-work-now",
        "closing-work",
        "legacy-work",
        "unicode",
    ] {
        let original = std::fs::read(fixtures.join(format!("{name}.jsonl"))).unwrap();
        std::fs::write(directory.0.join(format!("{name}.jsonl")), &original).unwrap();
        let info = store.get(name).unwrap();
        if info.closed {
            store.push(name, &[], Direction::Back, true).unwrap();
        } else {
            store.push(name, &[], Direction::Back, false).unwrap();
        }
        assert_eq!(
            std::fs::read(directory.0.join(format!("{name}.jsonl"))).unwrap(),
            original,
            "{name}"
        );
    }
    let ascii = (0..128)
        .map(|value| char::from_u32(value).unwrap())
        .collect::<String>();
    let fresh = Directory::new();
    let fresh_store = fresh.store();
    fresh_store.create("ascii", "encoding boundary").unwrap();
    fresh_store
        .push("ascii", &[ascii], Direction::Back, false)
        .unwrap();
    assert_eq!(
        std::fs::read(fresh.0.join("ascii.jsonl")).unwrap(),
        std::fs::read(fixtures.join("ascii.jsonl")).unwrap()
    );
}

#[test]
fn close_reopen_idempotence_and_drain() {
    let directory = Directory::new();
    let store = directory.store();
    store.create("closing-work", "finish it").unwrap();
    let closed = store
        .push(
            "closing-work",
            &strings(&["one", "two"]),
            Direction::Back,
            true,
        )
        .unwrap();
    assert!(closed.closed);
    let persisted = std::fs::read(&closed.path).unwrap();
    assert_eq!(
        store
            .push("closing-work", &[], Direction::Back, true)
            .unwrap(),
        closed
    );
    for items in [strings(&["late"]), Vec::new()] {
        let error = store
            .push("closing-work", &items, Direction::Back, false)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Closed);
        assert_eq!(error.message, "queue: 'closing-work' is closed");
        assert_eq!(std::fs::read(&closed.path).unwrap(), persisted);
    }
    store.dispose().unwrap();
    let restored = directory.store();
    assert!(restored.get("closing-work").unwrap().closed);
    let taken = restored
        .try_take("closing-work", 5, Direction::Front)
        .unwrap();
    assert_eq!(taken.items, strings(&["one", "two"]));
    assert!(taken.info.unwrap().closed);
    let done = restored
        .try_take("closing-work", 1, Direction::Front)
        .unwrap();
    assert!(done.acquired && done.items.is_empty() && done.info.unwrap().closed);
}

#[test]
fn metadata_legacy_null_unknown_and_malformed_boundaries() {
    let directory = Directory::new();
    let store = directory.store();
    let path = directory.0.join("work.jsonl");
    for (data, expected) in [
        ("", "queue: could not read metadata line"),
        ("{}", "queue: could not read metadata line"),
        ("null\n", "queue: metadata is null"),
        ("{}\n", "queue: malformed JSONL"),
        (
            "{\"name\":null}\n",
            "queue: metadata name does not match path",
        ),
        (
            "{\"name\":\"other\"}\n",
            "queue: metadata name does not match path",
        ),
        (
            "{\"name\":\"work\",\"extra\":1}\n",
            "queue: malformed JSONL",
        ),
        (
            "{\"name\":\"work\",\"closed\":null}\n",
            "queue: malformed JSONL",
        ),
        (
            "{\"name\":42,\"name\":\"work\"}\n",
            "queue: malformed JSONL",
        ),
        (
            "{\"name\":\"work\",\"closed\":null,\"closed\":false}\n",
            "queue: malformed JSONL",
        ),
        ("{\"name\":\"work\"}\nnull\n", "queue: item is null"),
        ("{\"name\":\"work\"}\n42\n", "queue: malformed JSONL"),
        ("{\"name\":\"work\"}\n\n", "queue: malformed JSONL"),
    ] {
        std::fs::write(&path, data).unwrap();
        assert_eq!(store.get("work").unwrap_err().message, expected, "{data:?}");
        let listed = store.list().unwrap_err();
        assert_eq!(listed.message, "queue: could not read 'work'");
        assert_eq!(listed.inner.unwrap().message, expected);
    }
    std::fs::write(
        &path,
        "{\"name\":\"other\",\"name\":\"work\",\"description\":null}\r\n\"legacy\"",
    )
    .unwrap();
    let info = store.get("work").unwrap();
    assert!(!info.closed);
    assert_eq!((info.description.as_str(), info.size), ("", 1));
    store
        .push("work", &strings(&["accepted"]), Direction::Back, false)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "{\"name\":\"work\"}\n\"legacy\"\n\"accepted\"\n"
    );
}

#[test]
fn validation_errors_precede_file_access_and_keep_bytes() {
    let directory = Directory::new();
    let store = directory.store();
    for name in ["", "A", "a--b", "-a", "a-", "a/b", "é", "a_b", "a\n"] {
        assert_eq!(
            store.get(name).unwrap_err().message,
            format!(
                "queue: name '{name}' must contain only lowercase ASCII alphanumeric words separated by hyphens"
            )
        );
    }
    assert_eq!(store.get("missing").unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(
        store
            .try_take("missing", 0, Direction::Invalid(5))
            .unwrap_err()
            .message,
        "queue: count 0 must be positive"
    );
    assert_eq!(
        store
            .push("A", &[], Direction::Invalid(5), false)
            .unwrap_err()
            .message,
        "queue: direction '5' must be front or back"
    );
    std::fs::create_dir(directory.0.join("directory.jsonl")).unwrap();
    assert_eq!(
        store.create("directory", "").unwrap_err().kind,
        ErrorKind::Io
    );
    std::fs::remove_dir(directory.0.join("directory.jsonl")).unwrap();
    store.create("work", "").unwrap();
    assert_eq!(
        store.create("work", "").unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    let before = std::fs::read(directory.0.join("work.jsonl")).unwrap();
    assert_eq!(
        store
            .push_nullable("work", &[None], Direction::Back, false)
            .unwrap_err()
            .message,
        "queue: items cannot contain null"
    );
    assert_eq!(
        std::fs::read(directory.0.join("work.jsonl")).unwrap(),
        before
    );
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    store.dispose().unwrap();
    assert_eq!(
        store
            .push("work", &[], Direction::Back, false)
            .unwrap_err()
            .kind,
        ErrorKind::Disposed
    );
    assert_eq!(store.list().unwrap_err().kind, ErrorKind::Disposed);
}

#[test]
fn maximum_bytes_exact_and_oversize_is_nonmutating() {
    let directory = Directory::new();
    let store = directory.store();
    let info = store.create("work", "").unwrap();
    let metadata_size = std::fs::read(&info.path).unwrap().len();
    let item = "a".repeat(MAXIMUM_FILE_BYTES - metadata_size - 3);
    store.push("work", &[item], Direction::Back, false).unwrap();
    assert_eq!(
        std::fs::metadata(&info.path).unwrap().len(),
        MAXIMUM_FILE_BYTES as u64
    );
    assert_eq!(store.get("work").unwrap().size, 1);
    assert_eq!(
        store
            .push("work", &strings(&[""]), Direction::Back, false)
            .unwrap_err()
            .message,
        "queue: file exceeds 16777216 bytes"
    );
    assert_eq!(
        std::fs::metadata(&info.path).unwrap().len(),
        MAXIMUM_FILE_BYTES as u64
    );
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    let mut oversized = std::fs::OpenOptions::new()
        .append(true)
        .open(&info.path)
        .unwrap();
    std::io::Write::write_all(&mut oversized, b" ").unwrap();
    assert_eq!(
        store.get("work").unwrap_err().message,
        "queue: file exceeds 16777216 bytes"
    );
}

#[test]
fn selected_root_never_borrows_missing_queue_and_modes_are_private() {
    let directory = Directory::new();
    let current =
        SelectedSessionRoot::new(directory.0.join("current"), RootOrigin::Current).unwrap();
    let legacy = SelectedSessionRoot::new(directory.0.join("legacy"), RootOrigin::Legacy).unwrap();
    let new_store = QueueStore::for_session(&current, "queues").unwrap();
    new_store.create("work", "new").unwrap();
    new_store
        .push("work", &strings(&["new"]), Direction::Back, false)
        .unwrap();
    let old_store = QueueStore::for_session(&legacy, "queues").unwrap();
    assert_eq!(old_store.get("work").unwrap_err().kind, ErrorKind::NotFound);
    old_store.create("work", "legacy").unwrap();
    old_store
        .push("work", &strings(&["old"]), Direction::Back, false)
        .unwrap();
    assert_eq!(
        new_store
            .try_take("work", 1, Direction::Front)
            .unwrap()
            .items,
        strings(&["new"])
    );
    assert_eq!(
        old_store
            .try_take("work", 1, Direction::Front)
            .unwrap()
            .items,
        strings(&["old"])
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(old_store.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(old_store.directory().join("work.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn inventory_replays_sorted_nonempty_and_records_zero_updates() {
    let directory = Directory::new();
    let store = directory.store();
    for name in ["zeta", "alpha", "empty"] {
        store.create(name, name).unwrap();
    }
    store
        .push("zeta", &strings(&["one", "two"]), Direction::Back, false)
        .unwrap();
    store
        .push("alpha", &strings(&["one"]), Direction::Back, false)
        .unwrap();
    let inventory = Arc::new(Recording::default());
    store.attach_inventory(owner(), inventory.clone()).unwrap();
    let initial = inventory.registered.lock().unwrap().clone();
    assert_eq!(
        initial
            .iter()
            .map(|state| state.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(initial[1].item_count, 2);
    assert_eq!(initial[0].owner, owner());
    store.try_take("alpha", 1, Direction::Front).unwrap();
    assert_eq!(
        inventory.updates.lock().unwrap().last().unwrap().item_count,
        0
    );
    assert!(store.attach_inventory(owner(), inventory.clone()).is_err());
    store.dispose().unwrap();
    assert_eq!(*inventory.unregisters.lock().unwrap(), 1);
}

struct BlockingRegister {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    recorded: Recording,
}
impl Inventory for BlockingRegister {
    fn register_owner(&self, queues: &[QueueState]) -> Result<()> {
        self.recorded.register_owner(queues)?;
        self.entered.wait();
        self.release.wait();
        Ok(())
    }
    fn update(&self, state: QueueState) -> Result<()> {
        self.recorded.update(state)
    }
    fn unregister_owner(&self) -> Result<()> {
        self.recorded.unregister_owner()
    }
}

#[test]
fn attach_race_coalesces_pending_mutations_including_empty() {
    let directory = Directory::new();
    let store = directory.store();
    store.create("work", "tasks").unwrap();
    let inventory = Arc::new(BlockingRegister {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
        recorded: Recording::default(),
    });
    let attaching = {
        let store = store.clone();
        let inventory = inventory.clone();
        std::thread::spawn(move || store.attach_inventory(owner(), inventory))
    };
    inventory.entered.wait();
    store
        .push("work", &strings(&["one"]), Direction::Back, false)
        .unwrap();
    store
        .push("work", &strings(&["two"]), Direction::Back, false)
        .unwrap();
    store.try_take("work", 2, Direction::Front).unwrap();
    inventory.release.wait();
    attaching.join().unwrap().unwrap();
    let updates = inventory.recorded.updates.lock().unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].item_count, 0);
}

#[test]
fn disposal_during_attach_unregisters_after_register_finishes() {
    let directory = Directory::new();
    let store = directory.store();
    let inventory = Arc::new(BlockingRegister {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
        recorded: Recording::default(),
    });
    let attaching = {
        let store = store.clone();
        let inventory = inventory.clone();
        std::thread::spawn(move || store.attach_inventory(owner(), inventory))
    };
    inventory.entered.wait();
    store.dispose().unwrap();
    assert_eq!(*inventory.recorded.unregisters.lock().unwrap(), 0);
    inventory.release.wait();
    assert_eq!(
        attaching.join().unwrap().unwrap_err().kind,
        ErrorKind::Disposed
    );
    assert_eq!(*inventory.recorded.unregisters.lock().unwrap(), 1);
}

struct FailingRegister;
impl Inventory for FailingRegister {
    fn register_owner(&self, _: &[QueueState]) -> Result<()> {
        Err(Error::new(ErrorKind::Inventory, "registration failed"))
    }
    fn update(&self, _: QueueState) -> Result<()> {
        unreachable!()
    }
    fn unregister_owner(&self) -> Result<()> {
        unreachable!()
    }
}
#[test]
fn failed_registration_can_be_retried() {
    let directory = Directory::new();
    let store = directory.store();
    assert_eq!(
        store
            .attach_inventory(owner(), Arc::new(FailingRegister))
            .unwrap_err()
            .message,
        "registration failed"
    );
    store
        .attach_inventory(owner(), Arc::new(Recording::default()))
        .unwrap();
}

struct BlockingUpdate {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}
impl Inventory for BlockingUpdate {
    fn register_owner(&self, _: &[QueueState]) -> Result<()> {
        Ok(())
    }
    fn update(&self, _: QueueState) -> Result<()> {
        self.entered.wait();
        self.release.wait();
        Ok(())
    }
    fn unregister_owner(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_take_observer_after_acquisition_preserves_owned_mutation() {
    let directory = Directory::new();
    let store = directory.store();
    store.create("work", "").unwrap();
    store
        .push("work", &strings(&["one"]), Direction::Back, false)
        .unwrap();
    let inventory = Arc::new(BlockingUpdate {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
    });
    store.attach_inventory(owner(), inventory.clone()).unwrap();
    let take = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .take("work", 1, Direction::Front, &Cancel::default())
                .await
        })
    };
    tokio::task::yield_now().await;
    inventory.entered.wait();
    take.abort();
    assert!(take.await.unwrap_err().is_cancelled());
    inventory.release.wait();
    assert_eq!(store.get("work").unwrap().size, 0);
    assert_eq!(
        std::fs::read_to_string(directory.0.join("work.jsonl")).unwrap(),
        "{\"name\":\"work\",\"description\":\"\"}\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_take_and_contended_try_take_do_not_remove_items() {
    let directory = Directory::new();
    let store = directory.store();
    store.create("work", "").unwrap();
    let inventory = Arc::new(BlockingUpdate {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
    });
    store.attach_inventory(owner(), inventory.clone()).unwrap();
    let pushing = {
        let store = store.clone();
        std::thread::spawn(move || store.push("work", &strings(&["one"]), Direction::Back, false))
    };
    inventory.entered.wait();
    assert!(
        !store
            .try_take("work", 1, Direction::Front)
            .unwrap()
            .acquired
    );
    let cancellation = Arc::new(Cancel::default());
    let task = {
        let store = store.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            store
                .take("work", 1, Direction::Front, cancellation.as_ref())
                .await
        })
    };
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    inventory.release.wait();
    pushing.join().unwrap().unwrap();
    assert_eq!(store.get("work").unwrap().size, 1);
    let cancelled = Cancel::default();
    cancelled.cancel();
    assert_eq!(
        store
            .take("work", 1, Direction::Front, &cancelled)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::CancelledBeforeWait
    );
    assert_eq!(store.get("work").unwrap().size, 1);
}
