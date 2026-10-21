mod boundaries;

use parrot_storage::queue_files::{Direction, ErrorKind, QueueStore};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-t22-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> QueueStore {
        QueueStore::new(&self.0).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn lifecycle_preserves_jsonl_and_direction_order() {
    let directory = Directory::new();
    let store = directory.store();
    let created = store.create("build-work-now", "release tasks").unwrap();
    assert_eq!(
        std::fs::read_to_string(created.path).unwrap(),
        "{\"name\":\"build-work-now\",\"description\":\"release tasks\"}\n"
    );
    store
        .push(
            "build-work-now",
            &strings(&["one", "three"]),
            Direction::Unspecified,
            false,
        )
        .unwrap();
    store
        .push(
            "build-work-now",
            &strings(&["zero", "two"]),
            Direction::Front,
            false,
        )
        .unwrap();
    assert_eq!(
        store
            .try_take("build-work-now", 2, Direction::Back)
            .unwrap()
            .items,
        strings(&["three", "one"])
    );
    assert_eq!(
        store
            .try_take("build-work-now", 5, Direction::Unspecified)
            .unwrap()
            .items,
        strings(&["two", "zero"])
    );
    assert_eq!(store.get("build-work-now").unwrap().size, 0);
    assert_eq!(
        store
            .try_take("build-work-now", 1, Direction::Front)
            .unwrap_err()
            .kind,
        ErrorKind::Empty
    );
}
