use std::collections::VecDeque;

use parrot_storage::queue_files::{Direction, ErrorKind, MAXIMUM_FILE_BYTES, QueueStore};

use crate::support::{fixture_root, for_cases};

#[test]
fn queue_jsonl_fixed_malformed_duplicate_and_actual_16mib_boundaries() {
    let root = fixture_root("queue-format");
    let malformed = [
        b"{\"name\":\"work\"}".as_slice(),
        b"null\n".as_slice(),
        b"{}\n".as_slice(),
        b"{\"name\":\"other\"}\n".as_slice(),
        b"{\"name\":\"work\",\"extra\":1}\n".as_slice(),
        b"{\"name\":\"work\"}\nnull\n".as_slice(),
        b"{\"name\":\"work\"}\n42\n".as_slice(),
        b"{\"name\":\"work\"}\n\n".as_slice(),
        b"{\"name\":\"work\"}\n\xff\n".as_slice(),
        b"{\"name\":\"work\",\"description\":1,\"description\":\"later\"}\n".as_slice(),
    ];
    for (index, bytes) in malformed.into_iter().enumerate() {
        let store = QueueStore::new(root.join(format!("malformed-{index}"))).unwrap();
        let queue = store.create("work", "").unwrap();
        std::fs::write(&queue.path, bytes).unwrap();
        let error = store.get("work").unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::Queue,
            "case={index} bytes={bytes:02x?} error={error}"
        );
        assert_eq!(
            std::fs::read(&queue.path).unwrap(),
            bytes,
            "decoder must not rewrite malformed input case={index}"
        );
        store.dispose().unwrap();
    }
    let store = QueueStore::new(root.join("valid-duplicates")).unwrap();
    let queue = store.create("work", "").unwrap();
    std::fs::write(&queue.path, b"{\"name\":\"other\",\"name\":\"work\",\"description\":\"first\",\"description\":\"last\"}\n\"last item without LF\"").unwrap();
    let info = store.get("work").unwrap();
    assert_eq!(info.description, "last");
    assert!(!info.closed);
    let taken = store.try_take("work", 10, Direction::Front).unwrap();
    assert!(taken.acquired);
    assert_eq!(taken.items, vec!["last item without LF"]);
    let before = std::fs::read(&queue.path).unwrap();
    assert!(
        store
            .push_nullable("work", &[None], Direction::Back, false)
            .is_err()
    );
    assert_eq!(std::fs::read(&queue.path).unwrap(), before);
    assert!(store.try_take("work", 0, Direction::Front).is_err());
    assert!(
        store
            .push("work", &[], Direction::Invalid(99), false)
            .is_err()
    );
    for name in ["", "../escape", "Upper", "a--b", "a/b", "-a", "a-"] {
        assert!(
            store.create(name, "").is_err(),
            "invalid queue name={name:?}"
        );
    }
    let oversized = "x".repeat(MAXIMUM_FILE_BYTES + 1);
    assert!(
        store
            .push("work", &[oversized], Direction::Back, false)
            .is_err()
    );
    assert_eq!(
        std::fs::read(&queue.path).unwrap(),
        before,
        "oversize encode must preserve existing queue bytes"
    );
    let sparse = store.create("oversized-read", "").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&sparse.path)
        .unwrap()
        .set_len((MAXIMUM_FILE_BYTES + 1) as u64)
        .unwrap();
    let error = store.get("oversized-read").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Queue);
    assert!(
        error.message.contains("file exceeds 16777216 bytes"),
        "{error}"
    );
    store.dispose().unwrap();
}

#[test]
fn seeded_queue_operations_match_finite_deque_model_without_contention_retries() {
    for_cases("queue", |case, generator| {
        let store = QueueStore::new(fixture_root("queue-model")).unwrap();
        store.create("work", "generated model").unwrap();
        let mut expected = VecDeque::new();
        let mut closed = false;
        let mut trace = Vec::new();
        for operation in 0..32 {
            if generator.pick(2) == 0 {
                let count = generator.pick(4);
                let mut items = Vec::new();
                for index in 0..count {
                    let length = generator.pick(12);
                    items.push(format!("{operation}-{index}:{}", generator.text(length)));
                }
                let direction = match generator.pick(3) {
                    0 => Direction::Front,
                    1 => Direction::Back,
                    _ => Direction::Unspecified,
                };
                let close = generator.pick(8) == 0;
                trace.push(format!("push {direction:?} close={close} items={items:?}"));
                let actual = store.push("work", &items, direction, close);
                if closed && !(close && items.is_empty()) {
                    assert_eq!(
                        actual.unwrap_err().kind,
                        ErrorKind::Closed,
                        "{case} trace={trace:?}"
                    );
                } else {
                    actual.unwrap_or_else(|error| panic!("{case} trace={trace:?}: {error}"));
                    if !closed {
                        for item in items {
                            if direction == Direction::Front {
                                expected.push_front(item);
                            } else {
                                expected.push_back(item);
                            }
                        }
                        closed |= close;
                    }
                }
            } else {
                let count = 1 + generator.pick(5);
                let direction = match generator.pick(3) {
                    0 => Direction::Front,
                    1 => Direction::Back,
                    _ => Direction::Unspecified,
                };
                trace.push(format!("take {direction:?} count={count}"));
                let actual = store.try_take("work", count as i32, direction);
                if expected.is_empty() && !closed {
                    assert_eq!(
                        actual.unwrap_err().kind,
                        ErrorKind::Empty,
                        "{case} trace={trace:?}"
                    );
                } else {
                    let actual =
                        actual.unwrap_or_else(|error| panic!("{case} trace={trace:?}: {error}"));
                    assert!(
                        actual.acquired,
                        "single-owner model must not silently retry contention: {case} trace={trace:?}"
                    );
                    let mut removed = Vec::new();
                    for _ in 0..count {
                        let item = if direction == Direction::Back {
                            expected.pop_back()
                        } else {
                            expected.pop_front()
                        };
                        if let Some(item) = item {
                            removed.push(item);
                        } else {
                            break;
                        }
                    }
                    assert_eq!(actual.items, removed, "{case} trace={trace:?}");
                }
            }
            let info = store.get("work").unwrap();
            assert_eq!(info.size, expected.len(), "{case} trace={trace:?}");
            assert_eq!(info.closed, closed, "{case} trace={trace:?}");
        }
        store.push("work", &[], Direction::Back, true).unwrap();
        let remaining = store.try_take("work", i32::MAX, Direction::Front).unwrap();
        assert!(remaining.acquired, "{case}");
        assert_eq!(
            remaining.items,
            expected.into_iter().collect::<Vec<_>>(),
            "{case} trace={trace:?}"
        );
        assert!(remaining.info.unwrap().closed, "{case}");
        assert!(
            store
                .try_take("work", 1, Direction::Front)
                .unwrap()
                .items
                .is_empty(),
            "{case}"
        );
        store.dispose().unwrap();
    });
}
