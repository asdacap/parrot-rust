//! Typed scoped runtime ports. Address/profile/child permissions belong to T48.

use super::async_ops::{BoxFuture, Cancellation, OperationError};
use super::security::AccessSnapshot;

/// Bound to one calling agent scope, not a global cross-agent dispatcher.
pub trait AgentRuntime: Send + Sync {
    type Security: AccessSnapshot + 'static;
    type SpawnRequest: Send + 'static;
    type SpawnResult: Send + 'static;
    type SendRequest: Send + 'static;
    type SendResult: Send + 'static;
    type InterruptRequest: Send + 'static;
    type InterruptResult: Send + 'static;
    type StatusRequest: Send + 'static;
    type StatusResult: Send + 'static;
    type WaitRequest: Send + 'static;
    type WaitResult: Send + 'static;
    type Error: Send + Sync + 'static;

    /// Receives invocation-start security captured by the caller. Source child selection
    /// and resume resolve the live scoped policy lineage; forwarding this value is not
    /// proof of immutable child-policy binding.
    fn spawn<'a>(
        &'a self,
        request: Self::SpawnRequest,
        security: Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::SpawnResult, OperationError<Self::Error>>>;

    fn send<'a>(
        &'a self,
        request: Self::SendRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::SendResult, OperationError<Self::Error>>>;

    fn interrupt<'a>(
        &'a self,
        request: Self::InterruptRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::InterruptResult, OperationError<Self::Error>>>;

    fn status(&self, request: Self::StatusRequest) -> Result<Self::StatusResult, Self::Error>;

    fn wait<'a>(
        &'a self,
        request: Self::WaitRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::WaitResult, OperationError<Self::Error>>>;
}

pub trait TaskController: Send + Sync {
    type Update: Send + 'static;
    type Query: Send + 'static;
    type Snapshot: Send + 'static;
    type Error: Send + Sync + 'static;

    /// Returns the accepted count for this call before mutable arguments or files are read.
    /// Implementations reject stale batches and never expose uncommitted receipts.
    fn lookup_accepted<'a>(
        &'a self,
        identity: &'a crate::values::agent_tasks::TaskCallIdentity,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<usize>, OperationError<Self::Error>>>;

    /// Updates the scoped task graph and returns its resulting snapshot.
    fn update<'a>(
        &'a self,
        update: Self::Update,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Snapshot, OperationError<Self::Error>>>;

    fn snapshot(&self, query: Self::Query) -> BoxFuture<'_, Result<Self::Snapshot, Self::Error>>;
}

/// Distinct queue operations preserve their own typed requests and results.
pub trait AgentQueues: Send + Sync {
    type CreateRequest: Send + 'static;
    type CreateResult: Send + 'static;
    type InfoRequest: Send + 'static;
    type InfoResult: Send + 'static;
    type PushRequest: Send + 'static;
    type PushResult: Send + 'static;
    type TakeRequest: Send + 'static;
    type TakeResult: Send + 'static;
    type Error: Send + Sync + 'static;

    fn create<'a>(
        &'a self,
        request: Self::CreateRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::CreateResult, OperationError<Self::Error>>>;

    fn info<'a>(
        &'a self,
        request: Self::InfoRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::InfoResult, OperationError<Self::Error>>>;

    fn push<'a>(
        &'a self,
        request: Self::PushRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::PushResult, OperationError<Self::Error>>>;

    fn take<'a>(
        &'a self,
        request: Self::TakeRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::TakeResult, OperationError<Self::Error>>>;
}

pub trait SessionControl: Send + Sync {
    type CheckpointRequest: Send + 'static;
    type CheckpointResult: Send + 'static;
    type ReminderRequest: Send + 'static;
    type ReminderResult: Send + 'static;
    type ClearReminderRequest: Send + 'static;
    type ClearReminderResult: Send + 'static;
    type CompactRequest: Send + 'static;
    type CompactResult: Send + 'static;
    type Status: Send + 'static;
    type Error: Send + Sync + 'static;

    fn checkpoint<'a>(
        &'a self,
        request: Self::CheckpointRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::CheckpointResult, OperationError<Self::Error>>>;

    fn compact<'a>(
        &'a self,
        request: Self::CompactRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::CompactResult, OperationError<Self::Error>>>;

    fn set_reminder(
        &self,
        request: Self::ReminderRequest,
    ) -> Result<Self::ReminderResult, Self::Error>;

    fn clear_reminder(
        &self,
        request: Self::ClearReminderRequest,
    ) -> Result<Self::ClearReminderResult, Self::Error>;

    fn status(&self) -> Result<Self::Status, Self::Error>;
}
