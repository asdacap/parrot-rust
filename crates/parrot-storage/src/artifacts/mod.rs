//! Selected-session artifact and bounded output blob storage.
//!
//! Every internal path is derived from one immutable [`SelectedSessionRoot`];
//! no lookup or fallback is attempted when that resource is absent.

pub mod compaction;
mod image_inspection;
mod image_repository;
mod image_store;
mod metadata;

pub mod agent_output_file;
pub mod output_blobs;

pub use image_inspection::ImageInspection;
pub use image_repository::ImageArtifactRepository;
pub use image_store::{ImageArtifactStore, ImageLimits};
pub use metadata::ImageArtifactMetadata;

use std::fmt;
use std::io;

use parrot_domain::contracts::paths::SelectedSessionRoot;

pub use parrot_domain::contracts::paths::{InvalidSessionPath, SessionResourcePath};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Io,
    InvalidInput,
    InvalidData,
    NotFound,
    Conflict,
    Cancelled,
    Database,
    Unsupported,
    Cleanup,
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }
    pub(crate) fn with_source<E: std::error::Error + Send + Sync + 'static>(
        mut self,
        source: E,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }
    pub(crate) fn invalid_data(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidData, message)
    }
    pub(crate) fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }
    pub(crate) fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self::new(ErrorKind::Io, message).with_source(source)
    }
    pub(crate) fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "operation cancelled")
    }
    pub(crate) fn task_cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "A task was canceled.")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|err| err as &(dyn std::error::Error + 'static))
    }
}

pub(crate) fn internal_resource(
    root: &SelectedSessionRoot,
    relative: impl AsRef<std::path::Path>,
) -> Result<SessionResourcePath> {
    root.resolve_internal(relative)
        .map_err(|err| Error::new(ErrorKind::InvalidInput, err.to_string()))
}
