//! Errors from installing packaged skill files.

use std::fmt;

/// Source exception families relevant to the extraction caller's catch filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallMissingErrorKind {
    Io,
    Denied,
    InvalidOperation,
}

/// Preserves the backend's message without the caller's wrapper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallMissingError {
    pub kind: InstallMissingErrorKind,
    pub message: String,
}

impl fmt::Display for InstallMissingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InstallMissingError {}
