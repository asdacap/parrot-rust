//! Wait orchestration over the existing session-owned incoming-input registration.
use super::blockers::ActiveWorkReminder;
use crate::agent_turn::{
    lifecycle::{AgentSession, DrainBackend},
    services::TurnError,
};
use parrot_domain::contracts::async_ops::Cancellation;
use std::{sync::Arc, time::Duration};

pub const MINIMUM_DURATION_MILLISECONDS: u64 = 10_000;
pub const DEFAULT_DURATION_MILLISECONDS: u64 = 300_000;
pub const MAXIMUM_DURATION_MILLISECONDS: u64 = u32::MAX as u64 - 1;

/// Tool-facing adapters format argument errors; this runtime owns no tool schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitDuration(u64);
impl WaitDuration {
    pub fn parse(arguments: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).map_err(|error| error.to_string())?;
        let object = value
            .as_object()
            .ok_or("Tool arguments must be an object.")?;
        if object.keys().any(|key| key != "duration_ms") {
            return Err("Tool arguments contain an unexpected property.".into());
        }
        match object.get("duration_ms") {
            None => Ok(Self(DEFAULT_DURATION_MILLISECONDS)),
            Some(duration) => duration.as_u64().and_then(Self::new).ok_or_else(|| format!(
                "Tool argument 'duration_ms' must be an integer from {MINIMUM_DURATION_MILLISECONDS} through {MAXIMUM_DURATION_MILLISECONDS}."
            )),
        }
    }
    pub fn new(milliseconds: u64) -> Option<Self> {
        (MINIMUM_DURATION_MILLISECONDS..=MAXIMUM_DURATION_MILLISECONDS)
            .contains(&milliseconds)
            .then_some(Self(milliseconds))
    }
    pub fn milliseconds(self) -> u64 {
        self.0
    }
}

pub struct WaitService<B: DrainBackend> {
    pub session: AgentSession<B>,
    pub active_work: Arc<ActiveWorkReminder>,
}
impl<B: DrainBackend> WaitService<B> {
    pub async fn wait(
        &self,
        duration: WaitDuration,
        cancellation: &dyn Cancellation,
    ) -> Result<String, TurnError> {
        let activity = self
            .session
            .wait_for_incoming_input(Duration::from_millis(duration.0), cancellation)
            .await
            .map_err(|error| {
                if error.is_cancelled() {
                    TurnError::Cancelled
                } else {
                    TurnError::Service(error.to_string())
                }
            })?;
        if let Some(activity) = activity {
            return Ok(match activity.cause {
                Some(cause) => format!("wait interrupted due to {cause}"),
                None => "wait interrupted".into(),
            });
        }
        Ok(format!(
            "Wait timed out after {} ms.{}",
            duration.0,
            self.active_work.work_sections().await?
        ))
    }
}
