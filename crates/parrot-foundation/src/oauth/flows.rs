use super::{AuthError, CredentialStorage, OAuthClient};
use crate::credentials::Credential;
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::fmt::Write;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    Information,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthDiagnostic {
    pub category: &'static str,
    pub operation: &'static str,
    pub severity: DiagnosticSeverity,
    pub correlation_id: String,
    pub duration_ms: Option<u64>,
    pub error_code: Option<&'static str>,
}

pub trait AuthDiagnosticLog: Send + Sync {
    fn write(&self, event: AuthDiagnostic);
}

pub trait AuthOutput: Send + Sync {
    fn write_line<'a>(
        &'a self,
        line: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>>;
}

pub struct AuthFlows;
impl AuthFlows {
    pub fn store_api_key<'a>(
        store: &'a dyn CredentialStorage,
        provider_id: &'a str,
        key: &'a str,
        diagnostics: &'a dyn AuthDiagnosticLog,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            let started = Instant::now();
            let correlation_id = correlation_id()?;
            diagnostics.write(event("api_key_start", correlation_id.clone(), None, None));
            let result = store
                .set(provider_id, &Credential::for_api_key(key), cancellation)
                .await;
            diagnostics.write(event(
                if result.is_ok() {
                    "api_key_complete"
                } else {
                    "api_key_failure"
                },
                correlation_id,
                Some(elapsed_ms(started)),
                result.as_ref().err(),
            ));
            result
        })
    }

    pub fn oauth_login<'a>(
        oauth: &'a dyn OAuthClient,
        store: &'a dyn CredentialStorage,
        device: bool,
        output: &'a dyn AuthOutput,
        diagnostics: &'a dyn AuthDiagnosticLog,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            let started = Instant::now();
            let correlation_id = correlation_id()?;
            diagnostics.write(event("oauth_start", correlation_id.clone(), None, None));
            let result = async {
                let credential = if device {
                    let authorization = oauth.start_device_authorization(cancellation).await?;
                    output
                        .write_line(
                            &format!(
                                "  visit {} and enter code {}",
                                authorization.verification_url,
                                authorization.user_code.value()
                            ),
                            cancellation,
                        )
                        .await?;
                    oauth
                        .await_device_authorization(&authorization, cancellation)
                        .await?
                } else {
                    output
                        .write_line("  opening your browser to authorize...", cancellation)
                        .await?;
                    oauth.browser_login(cancellation).await?
                };
                store
                    .set("chatgpt", &Credential::for_oauth(credential), cancellation)
                    .await
            }
            .await;
            diagnostics.write(event(
                if result.is_ok() {
                    "oauth_complete"
                } else {
                    "oauth_failure"
                },
                correlation_id,
                Some(elapsed_ms(started)),
                result.as_ref().err(),
            ));
            result
        })
    }
}

fn event(
    operation: &'static str,
    correlation_id: String,
    duration_ms: Option<u64>,
    failure: Option<&OperationError<AuthError>>,
) -> AuthDiagnostic {
    AuthDiagnostic {
        category: "auth",
        operation,
        severity: if matches!(failure, Some(OperationError::Failed(_))) {
            DiagnosticSeverity::Error
        } else {
            DiagnosticSeverity::Information
        },
        correlation_id,
        duration_ms,
        error_code: failure.map(|failure| match failure {
            OperationError::Cancelled => "cancelled",
            OperationError::Failed(AuthError::Transport | AuthError::TransportWithMessage(_)) => {
                "http"
            }
            OperationError::Failed(AuthError::Io | AuthError::IoWithMessage(_)) => "io",
            OperationError::Failed(AuthError::InvalidOperation(_)) => "invalid_operation",
            OperationError::Failed(
                AuthError::ArgumentOutOfRange(_)
                | AuthError::ArgumentOutOfRangeWithParameter { .. },
            ) => "argument",
            OperationError::Failed(_) => "unexpected",
        }),
    }
}

fn correlation_id() -> OperationResult<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| {
        OperationError::Failed(AuthError::Message(
            "auth: cannot create correlation identifier",
        ))
    })?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut identifier = String::with_capacity(32);
    for byte in bytes {
        write!(&mut identifier, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(identifier)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
