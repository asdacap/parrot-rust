use parrot_cli::presenters::usage::{RuntimeUsage, RuntimeUsageTracker};
use parrot_domain::protocol::messages::parrot::SessionUsageSnapshot;

#[test]
fn usage_tracker_accepts_zero_then_only_strictly_newer_authoritative_replacements() {
    let mut tracker = RuntimeUsageTracker::new();
    assert_eq!(tracker.current(), RuntimeUsage::default());
    assert!(tracker.observe(&SessionUsageSnapshot::default()));
    assert!(!tracker.observe(&SessionUsageSnapshot {
        input_tokens: 99,
        ..Default::default()
    }));
    let mut snapshot = SessionUsageSnapshot {
        revision: 4,
        input_tokens: 2400,
        cached_input_tokens: 1200,
        output_tokens: 120,
        context_size: 8000,
        context_limit: 32000,
        input_cost: 0.001,
        output_cost: 0.002,
        ..Default::default()
    };
    assert!(tracker.observe(&snapshot));
    snapshot.input_tokens = 999;
    assert_eq!(
        tracker.current(),
        RuntimeUsage::new(2400, 1200, 120, 8000, 32000, 0.003)
    );
    assert!(!tracker.observe(&snapshot));
    snapshot.revision = 3;
    assert!(!tracker.observe(&snapshot));
    snapshot.revision = 5;
    snapshot.input_tokens = 10;
    assert!(tracker.observe(&snapshot));
    assert_eq!(tracker.current().input_tokens, 10);
    tracker.reset();
    assert_eq!(tracker.current(), RuntimeUsage::default());
    assert!(tracker.observe(&SessionUsageSnapshot::default()));
}

#[test]
fn usage_formats_tokens_cached_ratio_context_and_cost_source_branches() {
    let usage = RuntimeUsage::new(2400, 1200, 120, 8000, 32000, 0.003);
    assert_eq!(usage.format_tokens(), "+2.4ki +120o (+50.00% cache)");
    assert_eq!(usage.format_context(), "8k/32k");
    assert_eq!(usage.format_cost(), "$0.0030");
    assert_eq!(RuntimeUsage::default().format_tokens(), "");
    assert_eq!(RuntimeUsage::default().format_context(), "");
    assert_eq!(RuntimeUsage::default().format_cost(), "");
    assert_eq!(
        RuntimeUsage::new(0, 100, 1_000_000, 2000, 0, 1.25).format_tokens(),
        "+0i +1Mo"
    );
    assert_eq!(
        RuntimeUsage::new(0, 0, 0, 2000, 0, 0.01).format_context(),
        "2k"
    );
    assert_eq!(
        RuntimeUsage::new(0, 0, 0, 0, 32000, 0.01).format_context(),
        "0/32k"
    );
    assert_eq!(
        RuntimeUsage::new(0, 0, 0, 0, 0, 0.01).format_cost(),
        "$0.01"
    );
    assert_eq!(RuntimeUsage::new(0, 0, 0, 0, 0, -1.0).format_cost(), "");
    assert_eq!(
        RuntimeUsage::new(0, 0, 0, 0, 0, f64::NAN).format_cost(),
        "NaN"
    );
    assert_eq!(
        RuntimeUsage::new(0, 0, 0, 0, 0, f64::INFINITY).format_cost(),
        "Infinity"
    );
    assert_eq!(RuntimeUsage::format_rate(0.0, 0.0), "");
    assert_eq!(
        RuntimeUsage::format_rate(1234.0, 2_500_000.0),
        "1.2ki/s 2.5Mo/s"
    );
    assert_eq!(RuntimeUsage::format_rate(1.25, 0.2), "1.25i/s 0.2o/s");
}

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_cli::presenters::usage::{
    RateClock, RateDelay, RateInvalidation, RollingTokenRateRefreshLifecycle,
    RollingTokenRateWindow, TokenRate,
};
use parrot_cli::terminal_frames::CancellationToken;
use tonic::Status;

fn clock(time: Arc<Mutex<Duration>>) -> RateClock {
    Arc::new(move || *time.lock().unwrap())
}

#[test]
fn window_accumulates_independent_rates_and_expires_at_exact_boundary() {
    let window = RollingTokenRateWindow::new(Duration::ZERO);
    window.observe(Duration::ZERO, 300, 60);
    window.observe(Duration::ZERO, 30, 0);
    let expected = TokenRate {
        input_tokens_per_second: 11.0,
        output_tokens_per_second: 2.0,
    };
    assert_eq!(window.current(Duration::ZERO), expected);
    assert_eq!(window.current(Duration::from_secs(29)), expected);
    assert_eq!(
        window.current(Duration::from_secs(30)),
        TokenRate::default()
    );
}

#[test]
fn window_clears_after_long_clock_advance_and_reset() {
    let window = RollingTokenRateWindow::new(Duration::ZERO);
    window.observe(Duration::ZERO, 300, 60);
    assert_eq!(
        window.current(Duration::from_secs(30)),
        TokenRate::default()
    );
    window.observe(Duration::from_secs(30), 300, 60);
    window.reset(Duration::from_secs(30));
    assert_eq!(
        window.current(Duration::from_secs(30)),
        TokenRate::default()
    );
}

#[test]
fn window_uses_bucket_expiry_ignores_negative_deltas_and_saturates_totals() {
    let window = RollingTokenRateWindow::new(Duration::from_secs(10));
    assert_eq!(window.expiry_delay(Duration::from_secs(5)), None);
    window.observe(Duration::from_secs(5), 300, -99);
    assert_eq!(
        window.expiry_delay(Duration::from_secs(10)),
        Some(Duration::from_secs(30))
    );
    window.observe(Duration::from_millis(11500), 60, 30);
    assert_eq!(
        window.expiry_delay(Duration::from_millis(11500)),
        Some(Duration::from_millis(28500))
    );
    assert_eq!(
        window
            .current(Duration::from_secs(39))
            .input_tokens_per_second,
        12.0
    );
    assert_eq!(
        window.current(Duration::from_secs(40)),
        TokenRate {
            input_tokens_per_second: 2.0,
            output_tokens_per_second: 1.0
        }
    );
    assert_eq!(
        window.expiry_delay(Duration::from_secs(40)),
        Some(Duration::from_secs(1))
    );
    assert!(!window.has_retained_samples(Duration::from_secs(41)));
    window.observe(Duration::from_secs(41), i64::MAX, i64::MAX);
    window.observe(Duration::from_secs(41), i64::MAX, -1);
    assert_eq!(
        window
            .current(Duration::from_secs(41))
            .input_tokens_per_second,
        i64::MAX as f64 / 30.0
    );
    window.observe(Duration::from_secs(42), i64::MAX, 1);
    assert_eq!(
        window
            .current(Duration::from_secs(42))
            .output_tokens_per_second,
        i64::MAX as f64 / 30.0
    );
}

#[tokio::test]
async fn reset_cancels_pending_refresh_before_replacement_can_be_invalidated() {
    let time = Arc::new(Mutex::new(Duration::ZERO));
    let window = Arc::new(RollingTokenRateWindow::new(Duration::ZERO));
    let waiting = Arc::new(tokio::sync::Notify::new());
    let invalidations = Arc::new(AtomicUsize::new(0));
    let count = invalidations.clone();
    let invalidate: RateInvalidation = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    });
    let entered = waiting.clone();
    let delay: RateDelay = Arc::new(move |_, cancellation| {
        let entered = entered.clone();
        Box::pin(async move {
            entered.notify_one();
            cancellation.cancelled().await;
            Err(Status::cancelled("operation cancelled"))
        })
    });
    let lifecycle = RollingTokenRateRefreshLifecycle::new(
        window.clone(),
        clock(time.clone()),
        invalidate,
        delay,
    );
    window.observe(Duration::ZERO, 300, 60);
    lifecycle.ensure_refreshing(&CancellationToken::new());
    waiting.notified().await;
    lifecycle.reset().await.unwrap();
    *time.lock().unwrap() = Duration::from_secs(30);
    assert_eq!(invalidations.load(Ordering::SeqCst), 0);
    assert_eq!(
        window.current(Duration::from_secs(30)),
        TokenRate::default()
    );
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn sample_after_reset_boundary_survives_joining_old_refresh() {
    let time = Arc::new(Mutex::new(Duration::ZERO));
    let window = Arc::new(RollingTokenRateWindow::new(Duration::ZERO));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (started_delay, reached_delay, release_delay) = (
        started.clone(),
        cancellation_reached.clone(),
        release.clone(),
    );
    let delay: RateDelay = Arc::new(move |_, cancellation| {
        let (started, reached, release) = (
            started_delay.clone(),
            reached_delay.clone(),
            release_delay.clone(),
        );
        Box::pin(async move {
            started.notify_one();
            cancellation.cancelled().await;
            reached.notify_one();
            release.notified().await;
            Err(Status::cancelled("operation cancelled"))
        })
    });
    let lifecycle = RollingTokenRateRefreshLifecycle::new(
        window.clone(),
        clock(time),
        Arc::new(|_| Box::pin(async { Ok(()) })),
        delay,
    );
    window.observe(Duration::ZERO, 300, 60);
    lifecycle.ensure_refreshing(&CancellationToken::new());
    started.notified().await;
    let resetting = lifecycle.reset();
    cancellation_reached.notified().await;
    window.observe(Duration::ZERO, 150, 30);
    release.notify_one();
    resetting.await.unwrap();
    assert_eq!(
        window.current(Duration::ZERO),
        TokenRate {
            input_tokens_per_second: 5.0,
            output_tokens_per_second: 1.0
        }
    );
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_and_joins_outstanding_refresh() {
    let window = Arc::new(RollingTokenRateWindow::new(Duration::ZERO));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (started_delay, reached_delay, release_delay) = (
        started.clone(),
        cancellation_reached.clone(),
        release.clone(),
    );
    let delay: RateDelay = Arc::new(move |_, cancellation| {
        let (started, reached, release) = (
            started_delay.clone(),
            reached_delay.clone(),
            release_delay.clone(),
        );
        Box::pin(async move {
            started.notify_one();
            cancellation.cancelled().await;
            reached.notify_one();
            release.notified().await;
            Err(Status::cancelled("operation cancelled"))
        })
    });
    let lifecycle = RollingTokenRateRefreshLifecycle::new(
        window.clone(),
        Arc::new(|| Duration::ZERO),
        Arc::new(|_| Box::pin(async { Ok(()) })),
        delay,
    );
    window.observe(Duration::ZERO, 300, 60);
    lifecycle.ensure_refreshing(&CancellationToken::new());
    started.notified().await;
    let shutting_down = tokio::spawn(lifecycle.shutdown());
    cancellation_reached.notified().await;
    assert!(!shutting_down.is_finished());
    release.notify_one();
    shutting_down.await.unwrap().unwrap();
    lifecycle.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_expires_samples_once_without_duplicate_loop_and_can_restart() {
    let time = Arc::new(Mutex::new(Duration::ZERO));
    let window = Arc::new(RollingTokenRateWindow::new(Duration::ZERO));
    let invalidations = Arc::new(AtomicUsize::new(0));
    let invalidated = Arc::new(tokio::sync::Notify::new());
    let (count, done) = (invalidations.clone(), invalidated.clone());
    let invalidate: RateInvalidation = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        done.notify_one();
        Box::pin(async { Ok(()) })
    });
    let advance = time.clone();
    let delay: RateDelay = Arc::new(move |delay, _| {
        *advance.lock().unwrap() += delay;
        Box::pin(async {
            tokio::task::yield_now().await;
            Ok(())
        })
    });
    let lifecycle = RollingTokenRateRefreshLifecycle::new(
        window.clone(),
        clock(time.clone()),
        invalidate,
        delay,
    );
    let cancellation = CancellationToken::new();
    lifecycle.ensure_refreshing(&cancellation);
    assert_eq!(invalidations.load(Ordering::SeqCst), 0);
    window.observe(Duration::ZERO, 300, 60);
    lifecycle.ensure_refreshing(&cancellation);
    lifecycle.ensure_refreshing(&cancellation);
    invalidated.notified().await;
    lifecycle.shutdown().await.unwrap();
    assert_eq!(invalidations.load(Ordering::SeqCst), 1);
    assert_eq!(window.current(*time.lock().unwrap()), TokenRate::default());
    window.observe(*time.lock().unwrap(), 300, 60);
    lifecycle.ensure_refreshing(&cancellation);
    invalidated.notified().await;
    lifecycle.shutdown().await.unwrap();
    assert_eq!(invalidations.load(Ordering::SeqCst), 2);
}
