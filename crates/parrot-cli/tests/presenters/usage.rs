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

use std::time::Duration;

use parrot_cli::presenters::usage::{RollingTokenRateWindow, TokenRate};

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
fn window_clears_after_long_clock_advance() {
    let window = RollingTokenRateWindow::new(Duration::ZERO);
    window.observe(Duration::ZERO, 300, 60);
    assert_eq!(
        window.current(Duration::from_secs(30)),
        TokenRate::default()
    );
}

#[test]
fn window_uses_bucket_expiry_ignores_negative_deltas_and_saturates_totals() {
    let window = RollingTokenRateWindow::new(Duration::from_secs(10));
    window.observe(Duration::from_secs(5), 300, -99);
    window.observe(Duration::from_millis(11500), 60, 30);
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
