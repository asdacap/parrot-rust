//! The platform launch boundary of the scoped process owner.
//!
//! This contract neither captures nor grants filesystem authority. The host
//! supplies a launcher which consumes the invocation's existing security value.

use std::io;
use std::sync::Arc;

use parrot_domain::contracts::async_ops::BoxFuture;
use parrot_domain::contracts::security::AccessSnapshot;

use super::execution::{Execution, ProcessLifetime};

/// The platform/host boundary. Implementations must own partial startup until it
/// is published or cleaned up, including when this future is dropped. The real
/// native PTY startup future already has this ownership contract.
///
/// `security` is the original owned invocation capture, not a replacement policy
/// or a mutable provider. Selecting and enforcing the sandbox remain the
/// launcher's responsibility; this adapter does not authorize a launch.
pub trait CapturedProcessLauncher: Send + Sync + 'static {
    type Request: Send + 'static;
    type Security: AccessSnapshot + 'static;

    fn launch(
        &self,
        request: Self::Request,
        security: Self::Security,
        lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>>;
}
