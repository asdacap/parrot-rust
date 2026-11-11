//! Authoritative session usage replacement state and modeline formatting.

use parrot_domain::protocol::messages::parrot::SessionUsageSnapshot;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RuntimeUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub context_size: i64,
    pub context_limit: i64,
    pub cost: f64,
}

impl RuntimeUsage {
    pub const fn new(
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
        context_size: i64,
        context_limit: i64,
        cost: f64,
    ) -> Self {
        Self {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            context_size,
            context_limit,
            cost,
        }
    }

    pub fn format_tokens(&self) -> String {
        if self.input_tokens == 0 && self.output_tokens == 0 {
            return String::new();
        }
        let value = format!(
            "+{}i +{}o",
            format_token_count(self.input_tokens),
            format_token_count(self.output_tokens)
        );
        if self.cached_input_tokens > 0 && self.input_tokens > 0 {
            format!(
                "{value} (+{:.2}% cache)",
                self.cached_input_tokens as f64 / self.input_tokens as f64 * 100.0
            )
        } else {
            value
        }
    }

    pub fn format_context(&self) -> String {
        if self.context_limit > 0 {
            format!(
                "{}/{}",
                format_token_count(self.context_size),
                format_token_count(self.context_limit)
            )
        } else if self.context_size > 0 {
            format_token_count(self.context_size)
        } else {
            String::new()
        }
    }

    pub fn format_cost(&self) -> String {
        if self.cost <= 0.0 {
            String::new()
        } else if self.cost.is_nan() {
            "NaN".into()
        } else if self.cost.is_infinite() {
            "Infinity".into()
        } else if self.cost < 0.01 {
            format!("${:.4}", self.cost)
        } else {
            format!("${:.2}", self.cost)
        }
    }

    pub fn format_rate(input_tokens_per_second: f64, output_tokens_per_second: f64) -> String {
        if input_tokens_per_second > 0.0 || output_tokens_per_second > 0.0 {
            format!(
                "{}i/s {}o/s",
                format_token_rate(input_tokens_per_second),
                format_token_rate(output_tokens_per_second)
            )
        } else {
            String::new()
        }
    }
}

fn decimal(value: f64, precision: usize) -> String {
    let value = format!("{value:.precision$}");
    if value.contains('.') {
        value.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        value
    }
}

fn format_token_count(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{}M", decimal(count as f64 / 1_000_000.0, 1))
    } else if count >= 1_000 {
        format!("{}k", decimal(count as f64 / 1_000.0, 1))
    } else {
        count.to_string()
    }
}

fn format_token_rate(rate: f64) -> String {
    if rate.abs() >= 1_000_000.0 {
        format!("{}M", decimal(rate / 1_000_000.0, 1))
    } else if rate.abs() >= 1_000.0 {
        format!("{}k", decimal(rate / 1_000.0, 1))
    } else {
        decimal(rate, 2)
    }
}

/// Snapshots replace totals; provider-call deltas must not be added here.
#[derive(Clone, Debug, Default)]
pub struct RuntimeUsageTracker {
    snapshot: Option<SessionUsageSnapshot>,
    revision: u64,
}

impl RuntimeUsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> RuntimeUsage {
        self.snapshot
            .as_ref()
            .map_or_else(RuntimeUsage::default, |snapshot| {
                RuntimeUsage::new(
                    snapshot.input_tokens,
                    snapshot.cached_input_tokens,
                    snapshot.output_tokens,
                    snapshot.context_size,
                    snapshot.context_limit,
                    snapshot.input_cost + snapshot.output_cost,
                )
            })
    }

    pub fn observe(&mut self, snapshot: &SessionUsageSnapshot) -> bool {
        if self.snapshot.is_some() && snapshot.revision <= self.revision {
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        self.revision = snapshot.revision;
        true
    }

    pub fn reset(&mut self) {
        self.snapshot = None;
        self.revision = 0;
    }
}

use std::sync::Mutex;
use std::time::Duration;

const RATE_BUCKET_COUNT: usize = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenRate {
    pub input_tokens_per_second: f64,
    pub output_tokens_per_second: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct BucketSlot {
    bucket: u64,
    input_tokens: i64,
    output_tokens: i64,
}

#[derive(Debug, Default)]
struct RateWindowState {
    slots: [BucketSlot; RATE_BUCKET_COUNT],
    bucket: u64,
}

/// A synchronized thirty-second window. `now` is monotonic caller time; no
/// timer or executor is created by this value.
#[derive(Debug)]
pub struct RollingTokenRateWindow {
    origin: Duration,
    state: Mutex<RateWindowState>,
}

impl RollingTokenRateWindow {
    pub fn new(origin: Duration) -> Self {
        Self {
            origin,
            state: Mutex::new(RateWindowState::default()),
        }
    }

    pub fn current(&self, now: Duration) -> TokenRate {
        let mut state = self.state.lock().expect("rate window poisoned");
        Self::advance_core(&mut state, self.elapsed(now));
        let earliest = state.bucket.saturating_sub(RATE_BUCKET_COUNT as u64 - 1);
        let (mut input, mut output) = (0_i64, 0_i64);
        for slot in &state.slots {
            if slot.bucket >= earliest && slot.bucket <= state.bucket {
                input = add_non_negative(input, slot.input_tokens);
                output = add_non_negative(output, slot.output_tokens);
            }
        }
        TokenRate {
            input_tokens_per_second: input as f64 / RATE_BUCKET_COUNT as f64,
            output_tokens_per_second: output as f64 / RATE_BUCKET_COUNT as f64,
        }
    }

    pub fn observe(&self, now: Duration, input_tokens: i64, output_tokens: i64) {
        let mut state = self.state.lock().expect("rate window poisoned");
        Self::advance_core(&mut state, self.elapsed(now));
        let bucket = state.bucket;
        let slot = &mut state.slots[(bucket % RATE_BUCKET_COUNT as u64) as usize];
        if slot.bucket != bucket {
            *slot = BucketSlot {
                bucket,
                ..Default::default()
            };
        }
        slot.input_tokens = add_non_negative(slot.input_tokens, input_tokens);
        slot.output_tokens = add_non_negative(slot.output_tokens, output_tokens);
    }

    pub fn advance(&self, now: Duration) {
        let mut state = self.state.lock().expect("rate window poisoned");
        Self::advance_core(&mut state, self.elapsed(now));
    }

    fn elapsed(&self, now: Duration) -> Duration {
        now.saturating_sub(self.origin)
    }

    fn advance_core(state: &mut RateWindowState, elapsed: Duration) {
        let next = elapsed.as_secs();
        if next <= state.bucket {
            return;
        }
        if next - state.bucket >= RATE_BUCKET_COUNT as u64 {
            state.slots.fill(BucketSlot::default());
        } else {
            for bucket in state.bucket + 1..=next {
                state.slots[(bucket % RATE_BUCKET_COUNT as u64) as usize] = BucketSlot::default();
            }
        }
        state.bucket = next;
    }
}

fn add_non_negative(total: i64, addition: i64) -> i64 {
    if addition <= 0 {
        total
    } else {
        total.saturating_add(addition)
    }
}
