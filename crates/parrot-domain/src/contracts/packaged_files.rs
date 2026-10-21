//! The install-if-missing boundary used by packaged skill extraction.
//!
//! This declares the I/O obligation; it supplies no filesystem backend or grants.
//! Resource ordering, destination validation, path joining, missing-resource
//! validation and source-level error wrapping remain with the caller. Correlated
//! plan-artifact creation is a separate lifecycle, not covered by this port.

use std::fmt;
use std::path::PathBuf;

use super::async_ops::BoxFuture;
use super::security::AccessSnapshot;

/// One already-selected packaged resource, not an authorization token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallMissingRequest {
    pub target: PathBuf,
    pub contents: Vec<u8>,
    /// The caller derives this from the resource's parent `scripts` segment.
    /// On non-Windows systems the temporary file receives mode 0755 before
    /// publication; this flag does not change permissions on an existing target.
    pub executable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallMissingOutcome {
    Installed,
    /// Either an initial file existed or another installer published the file.
    /// Neither case permits overwriting the existing file or its local edits.
    AlreadyPresent,
}

/// Source exception families relevant to the extraction caller's catch filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallMissingErrorKind {
    Io,
    Denied,
    InvalidOperation,
}

impl InstallMissingErrorKind {
    /// Only these families enter the caller's packaged-write error wrapper.
    /// A missing embedded resource is an uncaught caller-side invalid operation.
    pub fn is_io_or_denied(self) -> bool {
        matches!(self, Self::Io | Self::Denied)
    }
}

/// Preserves the backend's message; the port does not apply the caller's wrapper.
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

/// Scoped packaged-file publication under an invocation's immutable capture.
///
/// A production adapter must resolve and authorize actual targets under the
/// supplied snapshot before mutation. It owns the inseparable source lifecycle:
/// preserve a preexisting file; create parent directories; write a random
/// same-directory temporary file; apply the requested executable mode; and
/// publish without overwrite. If publication loses to a concurrently created
/// file, it must preserve that winner and delete its own temporary file.
///
/// This does not promise rollback or temporary-file cleanup for every other
/// copy, permission or publication failure: the source makes no such promise.
/// Returning an outcome or constructing a request confers no filesystem access.
/// There is no unrestricted default implementation, read-to-write conversion,
/// implicit refollow policy, progress callback or cancellation boundary here.
/// The source extraction has no cancellation parameter; callers must await and
/// join the adapter's owned I/O. Dropping an observer future does not establish
/// cancellation, completion or rollback of that work.
pub trait PackagedFileInstaller: Send + Sync {
    type Security: AccessSnapshot;

    fn install_missing<'a>(
        &'a self,
        request: &'a InstallMissingRequest,
        security: &'a Self::Security,
    ) -> BoxFuture<'a, Result<InstallMissingOutcome, InstallMissingError>>;
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::Path;

    use super::*;
    use crate::contracts::paths::ResolvedRead;

    struct DeniedSnapshot;

    impl AccessSnapshot for DeniedSnapshot {
        type Error = &'static str;

        fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
            Err("compile fixture grants no access")
        }

        fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
            Err("compile fixture grants no access")
        }
    }

    // A denied-only compile fixture, never a filesystem or authorization backend.
    struct DeniedInstaller;

    impl PackagedFileInstaller for DeniedInstaller {
        type Security = DeniedSnapshot;

        fn install_missing<'a>(
            &'a self,
            _: &'a InstallMissingRequest,
            _: &'a Self::Security,
        ) -> BoxFuture<'a, Result<InstallMissingOutcome, InstallMissingError>> {
            Box::pin(async {
                Err(InstallMissingError {
                    kind: InstallMissingErrorKind::Denied,
                    message: "compile fixture grants no access".into(),
                })
            })
        }
    }

    #[test]
    fn installer_future_is_send_and_borrows_request_and_capture() {
        fn assert_send<T: Future + Send>(_: T) {}

        let installer = DeniedInstaller;
        let security = DeniedSnapshot;
        let request = InstallMissingRequest {
            target: PathBuf::from("compile-fixture-only"),
            contents: vec![1, 2, 3],
            executable: true,
        };
        assert_send(installer.install_missing(&request, &security));
    }

    #[test]
    fn error_families_preserve_the_callers_catch_boundary_and_message() {
        assert!(InstallMissingErrorKind::Io.is_io_or_denied());
        assert!(InstallMissingErrorKind::Denied.is_io_or_denied());
        assert!(!InstallMissingErrorKind::InvalidOperation.is_io_or_denied());
        let error = InstallMissingError {
            kind: InstallMissingErrorKind::Io,
            message: "original I/O detail".into(),
        };
        assert_eq!(error.to_string(), "original I/O detail");
    }
}
