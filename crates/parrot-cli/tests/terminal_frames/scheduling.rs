use std::cell::{Cell, RefCell};
use std::future::{Future, poll_fn, ready};
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Duration;

use parrot_cli::terminal_frames::{
    CancellationToken, LiveUpdateScheduler, TerminalSpinner, spinner_text,
};

fn receive<T>(receiver: &mpsc::Receiver<T>) -> T {
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("callback did not arrive")
}

#[test]
fn token_clones_and_links_share_cancellation_without_cancelling_the_parent() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CancellationToken>();
    let parent = CancellationToken::new();
    let child = CancellationToken::linked(&parent);
    child.clone().cancel();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
    let child = CancellationToken::linked(&parent);
    let grandchild = CancellationToken::linked(&child);
    parent.cancel();
    assert!(grandchild.is_cancelled());
    assert_eq!(
        grandchild.check().unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn invalidate_coalesces_signals_until_run_consumes_them() {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    assert!(scheduler.invalidate());
    assert!(!scheduler.invalidate());
    let mut delays = 0;
    scheduler
        .run(
            &token,
            |_| {
                token.cancel();
                Ok(())
            },
            |interval, _| {
                assert_eq!(interval, Duration::from_nanos(33_333_300));
                delays += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(delays, 1);
}

#[test]
fn run_drains_invalidations_received_during_the_publish_interval() {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    scheduler.invalidate();
    let mut draws = 0;
    scheduler
        .run(
            &token,
            |_| {
                draws += 1;
                // The interval's invalidation was drained, so a new one can be queued.
                assert!(scheduler.invalidate());
                token.cancel();
                Ok(())
            },
            |_, _| {
                assert!(scheduler.invalidate());
                assert!(!scheduler.invalidate());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(draws, 1);
}

#[test]
fn run_draws_the_latest_state_at_publish_time() {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    let state = AtomicUsize::new(1);
    scheduler.invalidate();
    scheduler
        .run(
            &token,
            |_| {
                assert_eq!(state.load(Ordering::Acquire), 2);
                token.cancel();
                Ok(())
            },
            |_, _| {
                state.store(2, Ordering::Release);
                scheduler.invalidate();
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn run_returns_cleanly_when_cancelled_while_waiting() {
    let scheduler = LiveUpdateScheduler::default();
    let token = CancellationToken::default();
    let (started, waiting) = mpsc::channel();
    thread::scope(|scope| {
        let run = scope.spawn(|| {
            started.send(()).unwrap();
            scheduler.run(
                &token,
                |_| panic!("unexpected draw"),
                |_, _| panic!("unexpected delay"),
            )
        });
        receive(&waiting);
        token.cancel();
        run.join().unwrap().unwrap();
    });
}

#[test]
fn run_propagates_draw_exceptions() {
    let scheduler = LiveUpdateScheduler::new();
    scheduler.invalidate();
    let error = scheduler
        .run(
            &CancellationToken::new(),
            |_| Err(io::Error::other("draw failed")),
            |_, _| Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "draw failed");
}

#[test]
fn invalidations_during_draw_survive_for_the_next_interval() {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    let mut draws = 0;
    let mut delays = 0;
    scheduler.invalidate();
    scheduler
        .run(
            &token,
            |_| {
                draws += 1;
                if draws == 1 {
                    assert!(scheduler.invalidate());
                } else {
                    token.cancel();
                }
                Ok(())
            },
            |_, _| {
                delays += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!((draws, delays), (2, 2));
}

fn observe_scheduler_cancellation(cooperative_draw: bool) -> (Vec<String>, bool) {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    let events = RefCell::new(Vec::new());
    scheduler.invalidate();
    let result = scheduler.run(
        &token,
        |cancel| {
            events
                .borrow_mut()
                .push(format!("draw:{}", cancel.is_cancelled()));
            if cooperative_draw {
                cancel.check()
            } else {
                Ok(())
            }
        },
        |_, cancel| {
            events.borrow_mut().push("delay".to_owned());
            cancel.cancel();
            Ok(())
        },
    );
    (events.into_inner(), result.is_err())
}

#[test]
fn scheduler_successful_delay_invokes_cooperative_draw_with_cancelled_token() {
    let (events, error) = observe_scheduler_cancellation(true);
    assert_eq!(events, ["delay", "draw:true"]);
    assert!(!error);
}

#[test]
fn scheduler_successful_delay_invokes_admitted_draw_despite_cancellation() {
    let (events, error) = observe_scheduler_cancellation(false);
    assert_eq!(events, ["delay", "draw:true"]);
    assert!(!error);
}

#[test]
fn source_oracle_scheduler_cancellation_delegation_matches() {
    let rows: Vec<serde_json::Value> =
        include_str!("fixtures/scheduler-cancellation/observations.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(rows.len(), 2);
    for (mode, cooperative_draw) in [("cooperative", true), ("admitted", false)] {
        let (events, error) = observe_scheduler_cancellation(cooperative_draw);
        assert!(!error);
        let name = format!("scheduler-cancellation-{mode}");
        let expected = rows.iter().find(|row| row["name"] == name).unwrap();
        assert_eq!(
            serde_json::json!({"name": name, "events": events}),
            *expected
        );
    }
}

#[test]
fn scheduler_only_swallows_interruption_when_its_token_is_cancelled() {
    for (kind, cancelled, succeeds) in [
        (io::ErrorKind::Interrupted, false, false),
        (io::ErrorKind::Interrupted, true, true),
        (io::ErrorKind::Other, true, false),
    ] {
        let scheduler = LiveUpdateScheduler::new();
        let token = CancellationToken::new();
        scheduler.invalidate();
        let result = scheduler.run(
            &token,
            |_| panic!("unexpected draw"),
            |_, cancel| {
                if cancelled {
                    cancel.cancel();
                }
                Err(io::Error::new(kind, "delay failed"))
            },
        );
        assert_eq!(result.is_ok(), succeeds);
        if let Err(error) = result {
            assert_eq!(error.kind(), kind);
        }
    }
}

#[test]
fn scheduler_virtual_delay_barrier_coalesces_concurrent_updates() {
    let scheduler = LiveUpdateScheduler::new();
    let token = CancellationToken::new();
    let state = AtomicUsize::new(1);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    scheduler.invalidate();
    thread::scope(|scope| {
        let running = scope.spawn(|| {
            scheduler.run(
                &token,
                |_| {
                    assert_eq!(state.load(Ordering::Acquire), 9);
                    token.cancel();
                    Ok(())
                },
                |_, _| {
                    started_tx.send(()).unwrap();
                    receive(&release_rx.lock().unwrap());
                    Ok(())
                },
            )
        });
        receive(&started_rx);
        state.store(9, Ordering::Release);
        assert!(scheduler.invalidate());
        assert!(!scheduler.invalidate());
        release_tx.send(()).unwrap();
        running.join().unwrap().unwrap();
    });
}

#[derive(Default)]
struct TestWake(AtomicUsize);
impl Wake for TestWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::from(Arc::new(TestWake::default()));
    future.poll(&mut Context::from_waker(&waker))
}

fn finish<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let wake = Arc::new(TestWake::default());
    let waker = Waker::from(Arc::clone(&wake));
    for _ in 0..100 {
        wake.0.store(0, Ordering::Relaxed);
        if let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
            return result;
        }
        assert_ne!(
            wake.0.load(Ordering::Relaxed),
            0,
            "virtual executor stalled without a wake"
        );
    }
    panic!("virtual future did not complete");
}

async fn cancellation_delay(interval: Duration, token: CancellationToken) -> io::Result<()> {
    assert_eq!(interval, Duration::from_millis(80));
    token.cancelled().await;
    token.check()
}

#[test]
fn linked_cancellation_wakes_registered_futures_without_helper_threads() {
    let parent = CancellationToken::new();
    let child = CancellationToken::linked(&parent);
    let mut cancelled = Box::pin(child.cancelled());
    let wake = Arc::new(TestWake::default());
    let waker = Waker::from(Arc::clone(&wake));
    assert!(
        cancelled
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    parent.cancel();
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert!(poll_once(cancelled.as_mut()).is_ready());
}

#[test]
fn dropped_cancellation_future_unregisters_its_waker() {
    let token = CancellationToken::new();
    let wake = Arc::new(TestWake::default());
    let waker = Waker::from(Arc::clone(&wake));
    {
        let mut future = Box::pin(token.cancelled());
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    token.cancel();
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
}

#[test]
fn spinner_preserve_joins_idempotently_and_repeated_runs_restart_at_zero() {
    let frames = &RefCell::new(Vec::new());
    let counts = &RefCell::new(Vec::new());
    let joined = &Cell::new(false);
    for _ in 0..2 {
        joined.set(false);
        finish(TerminalSpinner::run(
            |index| index,
            |items, _| {
                counts.borrow_mut().push(items.len());
                frames.borrow_mut().extend(items);
                ready(Ok(()))
            },
            |interval, token| async move {
                let result = cancellation_delay(interval, token).await;
                joined.set(true);
                result
            },
            |preserve, _| async move {
                preserve.preserve().await?;
                assert!(joined.get());
                preserve.preserve().await
            },
            &CancellationToken::new(),
        ))
        .unwrap();
    }
    assert_eq!(*frames.borrow(), [0, 0]);
    assert_eq!(*counts.borrow(), [1, 1]);
}

#[test]
fn spinner_normal_end_and_cancel_clear_after_join_with_an_uncancelled_token() {
    for cancel_lifetime in [false, true] {
        let counts = &RefCell::new(Vec::new());
        let joined = &Cell::new(false);
        finish(TerminalSpinner::run(
            |index| index,
            |items, token| {
                token.check().unwrap();
                if items.is_empty() {
                    assert!(joined.get());
                    token.cancel();
                    token.check().unwrap();
                }
                counts.borrow_mut().push(items.len());
                ready(Ok(()))
            },
            |interval, token| async move {
                let result = cancellation_delay(interval, token).await;
                joined.set(true);
                result
            },
            |_, token| async move {
                if cancel_lifetime {
                    token.cancel();
                    token.check()
                } else {
                    Ok(())
                }
            },
            &CancellationToken::new(),
        ))
        .unwrap();
        assert_eq!(*counts.borrow(), [1, 0]);
    }
}

#[test]
fn spinner_lifetime_errors_clear_and_animation_errors_prevent_clear() {
    for animation_error in [false, true] {
        let counts = &RefCell::new(Vec::new());
        let result = finish(TerminalSpinner::run(
            |index| index,
            |items, _| {
                counts.borrow_mut().push(items.len());
                ready(Ok(()))
            },
            |interval, token| async move {
                if animation_error {
                    Err(io::Error::other("animation failed"))
                } else {
                    cancellation_delay(interval, token).await
                }
            },
            |_, _| ready(Err(io::Error::other("lifetime failed"))),
            &CancellationToken::new(),
        ));
        assert_eq!(
            result.unwrap_err().to_string(),
            if animation_error {
                "animation failed"
            } else {
                "lifetime failed"
            }
        );
        assert_eq!(
            *counts.borrow(),
            if animation_error { vec![1] } else { vec![1, 0] }
        );
    }
}

#[test]
fn spinner_does_not_swallow_unrelated_interruptions_or_non_cancellation_errors() {
    for (kind, cancel_lifetime, succeeds) in [
        (io::ErrorKind::Interrupted, false, false),
        (io::ErrorKind::Interrupted, true, true),
        (io::ErrorKind::Other, true, false),
    ] {
        let result = finish(TerminalSpinner::run(
            |index| index,
            |_, _| ready(Ok(())),
            cancellation_delay,
            |_, token| async move {
                if cancel_lifetime {
                    token.cancel();
                }
                Err(io::Error::new(kind, "lifetime error"))
            },
            &CancellationToken::new(),
        ));
        assert_eq!(result.is_ok(), succeeds);
    }
    let counts = &RefCell::new(Vec::new());
    let result = finish(TerminalSpinner::run(
        |index| index,
        |items, _| {
            counts.borrow_mut().push(items.len());
            ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "draw interrupted",
            )))
        },
        cancellation_delay,
        |_, _| ready(Ok(())),
        &CancellationToken::new(),
    ));
    assert_eq!(result.unwrap_err().to_string(), "draw interrupted");
    assert_eq!(*counts.borrow(), [1]);
}

#[test]
fn spinner_pending_first_draw_allows_lifetime_to_run_before_draw_completes() {
    let events = &RefCell::new(Vec::new());
    let release = &Cell::new(false);
    let result = finish(TerminalSpinner::run(
        |index| index,
        |items, _| {
            let events = &events;
            let release = &release;
            async move {
                if items.is_empty() {
                    events.borrow_mut().push("clear");
                    return Ok(());
                }
                events.borrow_mut().push("draw-start");
                poll_fn(|_| {
                    if release.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                events.borrow_mut().push("draw-end");
                Ok(())
            }
        },
        |interval, token| {
            assert_eq!(interval, Duration::from_millis(80));
            assert!(token.is_cancelled());
            events.borrow_mut().push("delay");
            ready(token.check())
        },
        |_, _| async {
            events.borrow_mut().push("lifetime");
            release.set(true);
            Ok(())
        },
        &CancellationToken::new(),
    ));
    result.unwrap();
    assert_eq!(
        *events.borrow(),
        ["draw-start", "lifetime", "draw-end", "delay", "clear"]
    );
}

#[test]
fn spinner_pending_first_draw_is_joined_before_preserve_returns() {
    let events = &RefCell::new(Vec::new());
    let release = Rc::new(Cell::new(false));
    let release_draw = Rc::clone(&release);
    let release_lifetime = Rc::clone(&release);
    let token = CancellationToken::new();
    let mut running = Box::pin(TerminalSpinner::run(
        |index| index,
        |_, _| {
            let release = Rc::clone(&release_draw);
            let events = &events;
            async move {
                events.borrow_mut().push("draw-start");
                poll_fn(|_| {
                    if release.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                events.borrow_mut().push("draw-end");
                Ok(())
            }
        },
        |interval, token| {
            assert_eq!(interval, Duration::from_millis(80));
            assert!(token.is_cancelled());
            events.borrow_mut().push("delay-after-cancel");
            cancellation_delay(interval, token)
        },
        |preserve, _| async move {
            events.borrow_mut().push("lifetime");
            let preserving = preserve.preserve();
            // Do not release the first draw until the test observes a pending join.
            preserving.await?;
            assert!(release_lifetime.get());
            events.borrow_mut().push("preserved");
            Ok(())
        },
        &token,
    ));
    assert!(poll_once(running.as_mut()).is_pending());
    assert_eq!(*events.borrow(), ["draw-start", "lifetime"]);
    release.set(true);
    finish(running).unwrap();
    assert_eq!(
        *events.borrow(),
        [
            "draw-start",
            "lifetime",
            "draw-end",
            "delay-after-cancel",
            "preserved"
        ]
    );
}

#[test]
fn spinner_precancelled_entry_still_invokes_draw_then_delay_before_lifetime() {
    let token = CancellationToken::new();
    token.cancel();
    let events = &RefCell::new(Vec::new());
    finish(TerminalSpinner::run(
        |index| index,
        |items, token| {
            assert!(!items.is_empty());
            assert!(token.is_cancelled());
            events.borrow_mut().push("draw");
            ready(Ok(()))
        },
        |interval, token| {
            assert_eq!(interval, Duration::from_millis(80));
            events.borrow_mut().push("delay");
            ready(token.check())
        },
        |preserve, _| async move {
            events.borrow_mut().push("lifetime");
            preserve.preserve().await
        },
        &token,
    ))
    .unwrap();
    assert_eq!(*events.borrow(), ["draw", "delay", "lifetime"]);
}

#[test]
fn spinner_advances_virtual_frames_then_preserves_without_late_repaint() {
    let indices = &RefCell::new(Vec::new());
    let delay_count = &Cell::new(0);
    let result = finish(TerminalSpinner::run(
        |index| index,
        |items, _| {
            indices.borrow_mut().extend(items);
            ready(Ok(()))
        },
        |interval, token| {
            assert_eq!(interval, Duration::from_millis(80));
            let count = delay_count.get();
            delay_count.set(count + 1);
            async move {
                if count == 0 {
                    Ok(())
                } else {
                    token.cancelled().await;
                    token.check()
                }
            }
        },
        |preserve, _| async move {
            assert_eq!(*indices.borrow(), [0, 1]);
            preserve.preserve().await
        },
        &CancellationToken::new(),
    ));
    result.unwrap();
    assert_eq!(*indices.borrow(), [0, 1]);
    assert_eq!(delay_count.get(), 2);
}

fn oracle_rows() -> Vec<serde_json::Value> {
    include_str!("fixtures/observations.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn source_oracle_spinner_text_rows_match() {
    let rows = oracle_rows();
    let mut compared = 0;
    for row in rows.iter().filter(|row| row["name"] == "spinner-text") {
        assert_eq!(
            spinner_text(
                row["activity"].as_str().unwrap(),
                row["frame"].as_u64().unwrap() as usize
            ),
            row["text"]
        );
        compared += 1;
    }
    assert_eq!(compared, 4);
}

#[test]
fn source_oracle_scheduler_lifecycle_events_and_errors_match() {
    for mode in [
        "coalesce",
        "drain",
        "latest",
        "during-draw",
        "cancel",
        "error",
    ] {
        let events = &RefCell::new(Vec::new());
        let state = &Cell::new("first");
        let mut draws = 0;
        let token = CancellationToken::new();
        let scheduler = LiveUpdateScheduler::new();
        if mode != "cancel" {
            events
                .borrow_mut()
                .push(format!("invalidate:{}", scheduler.invalidate()));
            events
                .borrow_mut()
                .push(format!("invalidate:{}", scheduler.invalidate()));
        } else {
            token.cancel();
        }
        let result = scheduler.run(
            &token,
            |token| {
                token.check()?;
                events.borrow_mut().push(format!("draw:{}", state.get()));
                draws += 1;
                if mode == "error" {
                    return Err(io::Error::other("draw failed"));
                }
                if mode == "during-draw" && draws == 1 {
                    events
                        .borrow_mut()
                        .push(format!("invalidate-draw:{}", scheduler.invalidate()));
                } else {
                    token.cancel();
                }
                Ok(())
            },
            |interval, _| {
                events
                    .borrow_mut()
                    .push(format!("delay:{}", interval.as_nanos() / 100));
                if mode == "drain" || mode == "latest" {
                    if mode == "latest" {
                        state.set("latest");
                    }
                    events
                        .borrow_mut()
                        .push(format!("invalidate-delay:{}", scheduler.invalidate()));
                    events
                        .borrow_mut()
                        .push(format!("invalidate-delay:{}", scheduler.invalidate()));
                }
                Ok(())
            },
        );
        let name = format!("scheduler-{mode}");
        let expected = oracle_rows()
            .into_iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert_eq!(
            serde_json::json!({"name": name, "events": events.borrow().clone(), "error": result.is_err()}),
            expected
        );
    }
}

#[derive(Default)]
struct Gate {
    open: Cell<bool>,
    waiter: RefCell<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.open.set(true);
        if let Some(waiter) = self.waiter.borrow_mut().take() {
            waiter.wake();
        }
    }

    async fn wait(&self) {
        poll_fn(|context| {
            if self.open.get() {
                Poll::Ready(())
            } else {
                *self.waiter.borrow_mut() = Some(context.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

#[test]
fn source_oracle_pending_first_draw_boundaries_match() {
    let rows: Vec<serde_json::Value> = include_str!("fixtures/boundaries/observations.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for mode in ["cancel", "preserve", "complete"] {
        let events = &RefCell::new(Vec::new());
        let release = &Gate::default();
        let delay_started = &Gate::default();
        let result = finish(TerminalSpinner::run(
            |index| index,
            |items, token| async move {
                if items.is_empty() {
                    events
                        .borrow_mut()
                        .push(format!("clear:{}", token.is_cancelled()));
                    return Ok(());
                }
                events.borrow_mut().push("draw-start".to_owned());
                let mut cancelled = Box::pin(token.cancelled());
                let mut released = Box::pin(release.wait());
                let result = poll_fn(|context| {
                    if cancelled.as_mut().poll(context).is_ready() {
                        Poll::Ready(token.check())
                    } else if released.as_mut().poll(context).is_ready() {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                if result.is_ok() {
                    events.borrow_mut().push("draw-complete".to_owned());
                }
                events.borrow_mut().push("draw-joined".to_owned());
                result
            },
            |interval, token| async move {
                events.borrow_mut().push("delay".to_owned());
                delay_started.release();
                cancellation_delay(interval, token).await
            },
            |preserve, token| async move {
                events.borrow_mut().push("lifetime".to_owned());
                if mode == "cancel" {
                    token.cancel();
                    token.cancelled().await;
                    token.check()?;
                }
                if mode == "complete" {
                    release.release();
                    delay_started.wait().await;
                }
                preserve.preserve().await?;
                events.borrow_mut().push("preserved".to_owned());
                Ok(())
            },
            &CancellationToken::new(),
        ));
        result.unwrap();
        let name = format!("spinner-pending-{mode}");
        let expected = rows.iter().find(|row| row["name"] == name).unwrap();
        assert_eq!(
            serde_json::json!({"name":name,"events":events.borrow().clone()}),
            *expected
        );
    }
}

#[test]
fn source_oracle_spinner_cancellation_delegation_matches() {
    let rows: Vec<serde_json::Value> =
        include_str!("fixtures/spinner-cancellation/observations.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(rows.len(), 2);
    for mode in ["before", "admitted"] {
        let events = &RefCell::new(Vec::new());
        let release = &Gate::default();
        let cancel = CancellationToken::new();
        if mode == "before" {
            cancel.cancel();
        }
        finish(TerminalSpinner::run(
            |index| index,
            |items, token| async move {
                if items.is_empty() {
                    events.borrow_mut().push("clear".to_owned());
                    return Ok(());
                }
                events
                    .borrow_mut()
                    .push(format!("draw:{}", token.is_cancelled()));
                if mode == "admitted" {
                    release.wait().await;
                }
                events.borrow_mut().push("draw-complete".to_owned());
                Ok(())
            },
            |interval, token| async move {
                events
                    .borrow_mut()
                    .push(format!("delay:{}", token.is_cancelled()));
                cancellation_delay(interval, token).await
            },
            |preserve, _| async move {
                events.borrow_mut().push("lifetime".to_owned());
                let preserving = preserve.preserve();
                events.borrow_mut().push("preserve-started".to_owned());
                release.release();
                preserving.await?;
                events.borrow_mut().push("preserved".to_owned());
                Ok(())
            },
            &cancel,
        ))
        .unwrap();
        let name = format!("spinner-cancellation-{mode}");
        let expected = rows.iter().find(|row| row["name"] == name).unwrap();
        assert_eq!(
            serde_json::json!({"name": name, "events": events.borrow().clone()}),
            *expected
        );
    }
}

#[test]
fn source_oracle_spinner_lifecycle_events_and_errors_match() {
    for mode in ["preserve", "end", "cancel", "error"] {
        let events = &RefCell::new(Vec::new());
        let result = finish(TerminalSpinner::run(
            |index| index,
            |items, token| {
                token.check().unwrap();
                events.borrow_mut().push(format!("draw:{}", items.len()));
                ready(Ok(()))
            },
            |interval, token| async move {
                events.borrow_mut().push("delay".to_owned());
                let result = cancellation_delay(interval, token).await;
                events.borrow_mut().push("joined".to_owned());
                result
            },
            |preserve, token| async move {
                events.borrow_mut().push("lifetime".to_owned());
                match mode {
                    "preserve" => {
                        preserve.preserve().await?;
                        events.borrow_mut().push("preserved".to_owned());
                        Ok(())
                    }
                    "cancel" => {
                        token.cancel();
                        token.cancelled().await;
                        token.check()
                    }
                    "error" => Err(io::Error::other("lifetime failed")),
                    _ => Ok(()),
                }
            },
            &CancellationToken::new(),
        ));
        let name = format!("spinner-{mode}");
        let expected = oracle_rows()
            .into_iter()
            .find(|row| row["name"] == name)
            .unwrap();
        assert_eq!(
            serde_json::json!({"name": name, "events": events.borrow().clone(), "error": result.is_err()}),
            expected
        );
    }
}
