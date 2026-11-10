use std::future::Future;

use parrot_domain::contracts::async_ops::Cancellation;
use tonic::{Response, Status};

use super::SlashFailure;

/// Observe cancellation at the native request boundary, without flattening the
/// explicit RPC failure returned by the generated client.
pub async fn native_call<T>(
    operation: impl Future<Output = Result<Response<T>, Status>> + Send,
    cancellation: &dyn Cancellation,
) -> Result<T, SlashFailure> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Status::cancelled("operation cancelled").into()),
        response = operation => response.map(Response::into_inner).map_err(Into::into),
    }
}
