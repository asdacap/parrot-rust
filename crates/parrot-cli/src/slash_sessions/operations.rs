use std::future::Future;

use parrot_domain::contracts::async_ops::Cancellation;
use tonic::{Response, Status};

use crate::dispatch::dialog::SlashDialog;

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

/// Adapt ordinary command failures through the one supplied dialog's loading
/// boundary. The Status used to terminate load never changes the original error
/// category returned to the registry/web adapter.
pub async fn load_command<T: Send>(
    dialog: &mut dyn SlashDialog,
    activity: &str,
    work: impl Future<Output = Result<T, SlashFailure>> + Send,
    cancellation: &dyn Cancellation,
) -> Result<T, SlashFailure> {
    let mut result = None;
    let loaded = dialog
        .load(
            activity,
            Box::pin(async {
                match work.await {
                    Ok(value) => {
                        result = Some(Ok(value));
                        Ok(())
                    }
                    Err(failure) => {
                        let status = match &failure {
                            SlashFailure::Rpc(status) => status.clone(),
                            SlashFailure::Command(message) => Status::internal(message.clone()),
                        };
                        result = Some(Err(failure));
                        Err(status)
                    }
                }
            }),
            cancellation,
        )
        .await;
    match (loaded, result) {
        (_, Some(Err(failure))) => Err(failure),
        (Err(failure), _) => Err(failure.into()),
        (Ok(()), Some(Ok(value))) => Ok(value),
        (Ok(()), None) => Err(SlashFailure::Command(
            "dialog load did not run supplied work".into(),
        )),
    }
}
