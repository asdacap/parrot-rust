use std::sync::{Arc, Barrier};

use parrot_storage::queue_files::{Direction, ErrorKind, QueueStore};

use crate::support::{Case, Generator, cases, fixture_root, seeds};

#[test]
fn seeded_queue_close_race_accounts_for_every_accepted_item_exactly_once() {
    exercise(1, &[0, 1], "T77 fixed producer/close regression");
    for seed in seeds() {
        let mut generator = Generator::new(seed);
        for index in 0..cases("queue_close_race") {
            let case = Case {
                domain: "queue_close_race",
                seed,
                index,
            };
            let producers = 1 + generator.pick(7);
            let mut order: Vec<_> = (0..=producers).collect();
            for index in (1..order.len()).rev() {
                let selected = generator.pick(index + 1);
                order.swap(index, selected);
            }
            exercise(producers, &order, &case.to_string());
        }
        println!(
            "T77_END domain=queue_close_race seed=0x{seed:016x} completed={}",
            cases("queue_close_race")
        );
    }
}

fn exercise(producers: usize, order: &[usize], context: &str) {
    let store = QueueStore::new(fixture_root("queue-close-race")).unwrap();
    store.create("work", "finite concurrent close").unwrap();
    let barrier = Arc::new(Barrier::new(producers + 2));
    let outcomes = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for &participant in order {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(scope.spawn(move || {
                barrier.wait();
                if participant == producers {
                    (None, store.push("work", &[], Direction::Back, true))
                } else {
                    let item = format!("item-{participant}");
                    (
                        Some(item.clone()),
                        store.push("work", &[item], Direction::Back, false),
                    )
                }
            }));
        }
        barrier.wait();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut expected = Vec::new();
    for (item, result) in &outcomes {
        match (item, result) {
            (Some(item), Ok(_)) => expected.push(item.clone()),
            (Some(_), Err(error)) => assert_eq!(
                error.kind,
                ErrorKind::Closed,
                "{context} order={order:?} outcomes={outcomes:?}"
            ),
            (None, result) => {
                result.as_ref().unwrap();
            }
        }
    }
    let taken = store.try_take("work", i32::MAX, Direction::Front).unwrap();
    let final_info = store.get("work").unwrap();
    let after_close = store
        .push("work", &["after-close".into()], Direction::Back, false)
        .unwrap_err();
    store.dispose().unwrap();
    let mut actual = taken.items;
    actual.sort();
    expected.sort();
    assert!(taken.acquired, "{context}");
    assert_eq!(
        actual, expected,
        "{context} order={order:?} outcomes={outcomes:?}"
    );
    assert!(final_info.closed, "{context}");
    assert_eq!(final_info.size, 0, "{context}");
    assert_eq!(after_close.kind, ErrorKind::Closed, "{context}");
}
