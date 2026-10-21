use crate::credentials::{Credential, CredentialError, CredentialStore, OAuthCredential, Secret};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::fmt;
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    Message(&'static str),
    Http {
        operation: &'static str,
        status: u16,
    },
    Credential(CredentialError),
    Transport,
    Io,
    InvalidOperation(&'static str),
    BrowserLaunch(Option<super::browser::BrowserLaunchError>),
    Format,
    ArgumentOutOfRange(&'static str),
    NotSupported(&'static str),
    FormatWithMessage(String),
    IoWithMessage(String),
    TransportWithMessage(String),
    ArgumentOutOfRangeWithParameter {
        message: String,
        parameter: Option<String>,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Http { operation, status } => {
                write!(formatter, "auth: {operation} returned HTTP {status}")
            }
            Self::Credential(error) => error.fmt(formatter),
            Self::Transport => formatter.write_str("auth: HTTP request failed"),
            Self::Io => formatter.write_str("auth: output failed"),
            Self::InvalidOperation(message) => formatter.write_str(message),
            Self::BrowserLaunch(_) => {
                formatter.write_str("auth: cannot open authorization URL; retry with --device")
            }
            Self::Format => {
                formatter.write_str("One of the identified items was in an invalid format.")
            }
            Self::ArgumentOutOfRange(message) | Self::NotSupported(message) => {
                formatter.write_str(message)
            }
            Self::FormatWithMessage(message)
            | Self::IoWithMessage(message)
            | Self::TransportWithMessage(message) => formatter.write_str(message),
            Self::ArgumentOutOfRangeWithParameter { message, parameter } => {
                formatter.write_str(message)?;
                if let Some(parameter) = parameter.as_deref().filter(|value| !value.is_empty()) {
                    write!(formatter, " (Parameter '{parameter}')")?;
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BrowserLaunch(Some(cause)) => Some(cause.error()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAuthorization {
    pub device_auth_id: Secret,
    pub user_code: Secret,
    pub verification_url: String,
    pub interval: Duration,
    pub expires_at: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthAccess {
    pub access_token: Secret,
    pub account_id: Option<String>,
}
impl From<OAuthCredential> for OAuthAccess {
    fn from(credential: OAuthCredential) -> Self {
        Self {
            access_token: credential.access_token,
            account_id: credential.account_id,
        }
    }
}

pub trait BrowserOpener: Send + Sync {
    fn open<'a>(
        &'a self,
        url: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>>;
}

pub trait OAuthClient: Send + Sync {
    fn now(&self) -> SystemTime;
    fn authorization_url(&self, redirect: &str, challenge: &str, state: &str) -> String;
    fn browser_login<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>>;
    fn start_device_authorization<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>>;
    fn await_device_authorization<'a>(
        &'a self,
        device: &'a DeviceAuthorization,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>>;
    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>>;
}

pub trait CredentialStorage: Send + Sync {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>>;
    fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>>;
}

fn map_store_error(error: OperationError<CredentialError>) -> OperationError<AuthError> {
    match error {
        OperationError::Cancelled => OperationError::Cancelled,
        OperationError::Failed(error) => OperationError::Failed(AuthError::Credential(error)),
    }
}

impl CredentialStorage for CredentialStore {
    fn get<'a>(
        &'a self,
        name: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            CredentialStore::get(self, name, cancellation)
                .await
                .map_err(map_store_error)
        })
    }
    fn set<'a>(
        &'a self,
        name: &'a str,
        credential: &'a Credential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            CredentialStore::set(self, name, credential, cancellation)
                .await
                .map_err(map_store_error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::AuthError;
    use std::error::Error;

    #[test]
    fn original_error_display_is_unchanged() {
        for (error, expected) in [
            (
                AuthError::Format,
                "One of the identified items was in an invalid format.",
            ),
            (AuthError::Io, "auth: output failed"),
            (AuthError::Transport, "auth: HTTP request failed"),
            (
                AuthError::ArgumentOutOfRange("range failure"),
                "range failure",
            ),
        ] {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn owned_error_messages_preserve_dynamic_input_without_inventing_a_source() {
        let message = format!("synthetic {} failure\nwith detail", "owned");
        for error in [
            AuthError::FormatWithMessage(message.clone()),
            AuthError::IoWithMessage(message.clone()),
            AuthError::TransportWithMessage(message.clone()),
        ] {
            assert_eq!(error.to_string(), message);
            assert_eq!(error.clone(), error);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn range_error_keeps_message_and_optional_parameter_separate() {
        for (parameter, expected) in [
            (
                Some("source_value"),
                "range failure (Parameter 'source_value')",
            ),
            (None, "range failure"),
            (Some(""), "range failure"),
        ] {
            let error = AuthError::ArgumentOutOfRangeWithParameter {
                message: String::from("range failure"),
                parameter: parameter.map(String::from),
            };
            assert_eq!(error.to_string(), expected);
            assert_eq!(error.clone(), error);
            assert!(error.source().is_none());
        }
    }
}
