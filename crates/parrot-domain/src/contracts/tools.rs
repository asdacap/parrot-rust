//! Tool invocation boundaries; concrete tools and scheduling belong to later tasks.

use super::async_ops::{BoxFuture, OperationError};
use super::security::{AccessSnapshot, InvocationContext};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelSafety {
    Sequential,
    ParallelSafe,
}

/// No default parallel safety or default successful execution. T47 captures
/// security at every invocation start and keeps ordered results across contiguous
/// parallel-safe batches; it must not hold blocking guards through await.
pub trait Tool: Send + Sync {
    type Definition: Send + Sync + 'static;
    type Invocation: Send + 'static;
    type Selection: Send + 'static;
    type Security: AccessSnapshot + 'static;
    type Output: Send + 'static;
    type Error: Send + Sync + 'static;

    fn definition(&self) -> &Self::Definition;

    /// Safety is invocation-dependent, not a single unconditional tool flag.
    fn parallel_safety(&self, invocation: &Self::Invocation) -> ParallelSafety;

    /// Explicitly declares whether this tool may run after turn interruption.
    fn enabled_after_interruption(&self) -> bool;

    fn invoke<'a>(
        &'a self,
        invocation: Self::Invocation,
        context: InvocationContext<'a, Self::Security, Self::Selection>,
    ) -> BoxFuture<'a, Result<Self::Output, OperationError<Self::Error>>>;
}
