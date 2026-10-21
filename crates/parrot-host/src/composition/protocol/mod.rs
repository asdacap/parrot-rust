//! The retained Parrot protocol service, independent of its native transports.

mod boundary;
pub mod models;
mod registry;
mod service;
pub mod stream;

pub use boundary::*;
pub use registry::UserSessionRegistry;
pub use service::ParrotService;

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum RpcCode {
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    FailedPrecondition = 9,
    Internal = 13,
    Unavailable = 14,
}

impl RpcCode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cancelled => "Cancelled",
            Self::Unknown => "Unknown",
            Self::InvalidArgument => "InvalidArgument",
            Self::NotFound => "NotFound",
            Self::AlreadyExists => "AlreadyExists",
            Self::PermissionDenied => "PermissionDenied",
            Self::FailedPrecondition => "FailedPrecondition",
            Self::Internal => "Internal",
            Self::Unavailable => "Unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcErrorOrigin {
    Rpc,
    UnhandledIo,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcError {
    pub code: RpcCode,
    pub message: String,
    origin: RpcErrorOrigin,
}
impl RpcError {
    pub fn new(code: RpcCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            origin: RpcErrorOrigin::Rpc,
        }
    }
    pub fn unhandled_io(message: impl Into<String>) -> Self {
        Self {
            code: RpcCode::Internal,
            message: message.into(),
            origin: RpcErrorOrigin::UnhandledIo,
        }
    }
    pub fn is_unhandled_io(&self) -> bool {
        self.origin == RpcErrorOrigin::UnhandledIo
    }
    pub fn cancelled() -> Self {
        Self::new(RpcCode::Cancelled, "The operation was canceled.")
    }
}
impl fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for RpcError {}
impl From<parrot_storage::artifacts::Error> for RpcError {
    fn from(error: parrot_storage::artifacts::Error) -> Self {
        use parrot_storage::artifacts::ErrorKind;
        let code = match error.kind {
            ErrorKind::Cancelled => RpcCode::Cancelled,
            ErrorKind::InvalidInput | ErrorKind::InvalidData | ErrorKind::Conflict => {
                RpcCode::InvalidArgument
            }
            _ => RpcCode::Internal,
        };
        Self::new(code, error.to_string())
    }
}
pub type Result<T> = std::result::Result<T, RpcError>;

pub(crate) fn check(
    cancellation: &dyn parrot_domain::contracts::async_ops::Cancellation,
) -> Result<()> {
    cancellation.check().map_err(|_| RpcError::cancelled())
}
