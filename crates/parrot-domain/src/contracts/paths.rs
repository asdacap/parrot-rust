//! Selected session identity and pure path policy, never filesystem lookup.

use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootOrigin {
    Current,
    Legacy,
}

/// An invalid selected root or app-generated internal relative path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSessionPath;

impl fmt::Display for InvalidSessionPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid selected session or internal resource path")
    }
}

impl std::error::Error for InvalidSessionPath {}

/// Immutable materialized session directory, not the launch workspace or XDG setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedSessionRoot {
    path: PathBuf,
    origin: RootOrigin,
}

impl SelectedSessionRoot {
    /// Validates an already materialized absolute path. The caller selects and
    /// materializes the directory; this performs no discovery, canonicalization,
    /// existence check, or filesystem access. These are lexical address
    /// invariants, not symlink confinement or security; T10/T36 must resolve and
    /// authorize actual physical targets.
    pub fn new(path: PathBuf, origin: RootOrigin) -> Result<Self, InvalidSessionPath> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(InvalidSessionPath);
        }
        Ok(Self { path, origin })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn origin(&self) -> RootOrigin {
        self.origin
    }

    /// Resolves only app-generated private resources within this exact root.
    /// Missing resources must never be searched for in another session root.
    /// This is not a resolver or authorization check for user file paths.
    pub fn resolve_internal(
        &self,
        relative: impl AsRef<Path>,
    ) -> Result<SessionResourcePath, InvalidSessionPath> {
        let relative = relative.as_ref();
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(InvalidSessionPath);
        }
        Ok(SessionResourcePath {
            selected_root: self.clone(),
            path: self.path.join(relative),
            relative: relative.to_path_buf(),
        })
    }
}

/// An internal path bound to the immutable root that resolved it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionResourcePath {
    selected_root: SelectedSessionRoot,
    path: PathBuf,
    relative: PathBuf,
}

impl SessionResourcePath {
    pub fn selected_root(&self) -> &SelectedSessionRoot {
        &self.selected_root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn relative_path(&self) -> &Path {
        &self.relative
    }
}

/// A resolver's report, not an authorization token. T10/T36 must resolve and
/// authorize the actual physical target, including symlinks. Constructing this
/// report confers no security and performs no filesystem lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRead {
    requested: PathBuf,
    actual: PathBuf,
}

impl ResolvedRead {
    pub fn new(requested: PathBuf, actual: PathBuf) -> Self {
        Self { requested, actual }
    }

    pub fn requested_path(&self) -> &Path {
        &self.requested
    }

    pub fn actual_path(&self) -> &Path {
        &self.actual
    }
}
