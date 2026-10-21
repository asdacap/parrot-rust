use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AgentUsageKey {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

impl AgentUsageKey {
    pub fn legacy() -> Self {
        Self {
            provider: "unknown".into(),
            model: "legacy".into(),
            effort: None,
        }
    }

    pub fn from_request(
        provider: impl Into<String>,
        model: impl Into<String>,
        actual_effort: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            effort: actual_effort,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageOverflow;

impl fmt::Display for UsageOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("usage integer arithmetic overflow")
    }
}

impl Error for UsageOverflow {}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentUsageTotals {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub tool_calls: i64,
    pub input_cost: f64,
    pub output_cost: f64,
}

impl AgentUsageTotals {
    pub fn total_cost(&self) -> f64 {
        self.input_cost + self.output_cost
    }

    pub fn add(&self, increment: &Self) -> Result<Self, UsageOverflow> {
        Ok(Self {
            input_tokens: self
                .input_tokens
                .checked_add(increment.input_tokens)
                .ok_or(UsageOverflow)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_add(increment.cached_input_tokens)
                .ok_or(UsageOverflow)?,
            output_tokens: self
                .output_tokens
                .checked_add(increment.output_tokens)
                .ok_or(UsageOverflow)?,
            tool_calls: self
                .tool_calls
                .checked_add(increment.tool_calls)
                .ok_or(UsageOverflow)?,
            input_cost: self.input_cost + increment.input_cost,
            output_cost: self.output_cost + increment.output_cost,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentCompletionUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentUsagePricing {
    pub input_price: f64,
    pub cached_input_price: Option<f64>,
    pub output_price: f64,
    pub context_window: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentUsageIncrement {
    pub key: AgentUsageKey,
    pub totals: AgentUsageTotals,
    pub context_size: Option<i64>,
    pub context_limit: Option<i64>,
}

impl AgentUsageIncrement {
    pub fn from_completion(
        key: AgentUsageKey,
        completed: AgentCompletionUsage,
        pricing: AgentUsagePricing,
    ) -> Self {
        let input_tokens = completed.input_tokens.max(0);
        let cached_input_tokens = completed.cached_input_tokens.clamp(0, input_tokens);
        let cached_input_price = pricing.cached_input_price.unwrap_or(pricing.input_price);
        let input_cost = ((input_tokens - cached_input_tokens) as f64 * pricing.input_price)
            + (cached_input_tokens as f64 * cached_input_price);
        Self {
            key,
            totals: AgentUsageTotals {
                input_tokens: completed.input_tokens,
                cached_input_tokens: completed.cached_input_tokens,
                output_tokens: completed.output_tokens,
                tool_calls: 0,
                input_cost: 0.0 + input_cost,
                output_cost: 0.0 + (completed.output_tokens as f64 * pricing.output_price),
            },
            context_size: Some(completed.input_tokens),
            context_limit: Some(pricing.context_window),
        }
    }

    pub fn from_tool_start(key: AgentUsageKey) -> Self {
        Self {
            key,
            totals: AgentUsageTotals {
                tool_calls: 1,
                ..AgentUsageTotals::default()
            },
            context_size: None,
            context_limit: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentUsageSnapshot {
    pub totals: AgentUsageTotals,
    pub models: BTreeMap<AgentUsageKey, AgentUsageTotals>,
}

impl AgentUsageSnapshot {
    pub fn add(&self, increment: &AgentUsageIncrement) -> Result<Self, UsageOverflow> {
        let totals = self.totals.add(&increment.totals)?;
        let model_totals = self
            .models
            .get(&increment.key)
            .copied()
            .unwrap_or_default()
            .add(&increment.totals)?;
        let mut models = self.models.clone();
        models.insert(increment.key.clone(), model_totals);
        Ok(Self { totals, models })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentStatistics {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub context_size: i64,
    pub context_limit: i64,
    pub input_cost: f64,
    pub output_cost: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentSessionStatisticsSnapshot {
    pub own: AgentUsageSnapshot,
    pub cumulative: AgentUsageSnapshot,
    pub context_size: i64,
    pub context_limit: i64,
    pub has_incomplete_legacy_tool_counts: bool,
}

impl AgentSessionStatisticsSnapshot {
    pub fn to_self_statistics(&self) -> AgentStatistics {
        AgentStatistics {
            input_tokens: self.own.totals.input_tokens,
            cached_input_tokens: self.own.totals.cached_input_tokens,
            output_tokens: self.own.totals.output_tokens,
            context_size: self.context_size,
            context_limit: self.context_limit,
            input_cost: self.own.totals.input_cost,
            output_cost: self.own.totals.output_cost,
        }
    }
}

#[derive(Debug, Default)]
pub struct AgentSessionStatistics {
    snapshot: Mutex<AgentSessionStatisticsSnapshot>,
}

impl AgentSessionStatistics {
    pub fn new(initial: AgentSessionStatisticsSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(initial),
        }
    }

    pub fn capture(&self) -> AgentSessionStatisticsSnapshot {
        self.snapshot
            .lock()
            .expect("statistics lock poisoned")
            .clone()
    }

    pub fn add_own(&self, increment: &AgentUsageIncrement) -> Result<(), UsageOverflow> {
        let mut snapshot = self.snapshot.lock().expect("statistics lock poisoned");
        let own = snapshot.own.add(increment)?;
        let cumulative = snapshot.cumulative.add(increment)?;
        *snapshot = AgentSessionStatisticsSnapshot {
            own,
            cumulative,
            context_size: increment.context_size.unwrap_or(snapshot.context_size),
            context_limit: increment.context_limit.unwrap_or(snapshot.context_limit),
            has_incomplete_legacy_tool_counts: snapshot.has_incomplete_legacy_tool_counts,
        };
        Ok(())
    }

    pub fn add_descendant(&self, increment: &AgentUsageIncrement) -> Result<(), UsageOverflow> {
        let mut snapshot = self.snapshot.lock().expect("statistics lock poisoned");
        snapshot.cumulative = snapshot.cumulative.add(increment)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedAgentLimitReached;

impl fmt::Display for RetainedAgentLimitReached {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("subagent retention limit reached")
    }
}

impl Error for RetainedAgentLimitReached {}

#[derive(Debug)]
struct BudgetState {
    capacity: i32,
    pending: i32,
    retained: i32,
}

#[derive(Clone, Debug)]
pub struct RetainedAgentBudget {
    state: Arc<Mutex<BudgetState>>,
}

impl RetainedAgentBudget {
    pub fn new(capacity: i32) -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState {
                capacity,
                pending: 0,
                retained: 0,
            })),
        }
    }

    pub fn reserve(&self) -> Result<RetainedAgentReservation, RetainedAgentLimitReached> {
        let mut state = self.state.lock().expect("retention budget lock poisoned");
        if i64::from(state.pending) + i64::from(state.retained) >= i64::from(state.capacity) {
            return Err(RetainedAgentLimitReached);
        }
        state.pending += 1;
        Ok(RetainedAgentReservation {
            owner: Arc::clone(&self.state),
            state: Arc::new(Mutex::new(ReservationState::Pending)),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationState {
    Pending,
    Retained,
    Released,
}

#[derive(Clone, Debug)]
pub struct RetainedAgentReservation {
    owner: Arc<Mutex<BudgetState>>,
    state: Arc<Mutex<ReservationState>>,
}

impl RetainedAgentReservation {
    pub fn commit(&self) {
        let mut owner = self.owner.lock().expect("retention budget lock poisoned");
        let mut state = self.state.lock().expect("reservation lock poisoned");
        if *state == ReservationState::Pending {
            *state = ReservationState::Retained;
            owner.pending -= 1;
            owner.retained += 1;
        }
    }

    pub fn rollback(&self) {
        let mut owner = self.owner.lock().expect("retention budget lock poisoned");
        let mut state = self.state.lock().expect("reservation lock poisoned");
        if *state == ReservationState::Pending {
            *state = ReservationState::Released;
            owner.pending -= 1;
        }
    }

    pub fn release(&self) {
        let mut owner = self.owner.lock().expect("retention budget lock poisoned");
        let mut state = self.state.lock().expect("reservation lock poisoned");
        if *state == ReservationState::Retained {
            *state = ReservationState::Released;
            owner.retained -= 1;
        }
    }
}
