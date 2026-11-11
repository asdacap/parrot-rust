//! The production wall-and-monotonic clock backed by Tokio's timer.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Cancelled, Clock};
use std::time::{Instant, SystemTime};

/// The normal Tokio adapter for the domain monotonic-clock contract.
/// Tests inject another `Clock` without changing the original deadline.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(Cancelled),
                _ = tokio::time::sleep_until(deadline.into()) => Ok(()),
            }
        })
    }
}
