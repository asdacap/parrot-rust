use super::{ApplicationRoots, native_platform, normalize};
use parrot_domain::contracts::paths::{
    PrimaryReadFailure, ReadOrigin, ReadPurpose, ResolvedRead, allows_legacy_fallback,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadCandidates {
    requested: PathBuf,
    primary: PathBuf,
    fallback: Option<PathBuf>,
    origin: ReadOrigin,
}

#[derive(Debug)]
pub struct ReadValue<T> {
    pub resolution: ResolvedRead,
    pub value: T,
}

impl ApplicationRoots {
    /// Plans general/historical reads only. Session internals must instead use
    /// their selected root. The plan neither grants access nor loads config.
    pub fn read_candidates(
        &self,
        requested: &Path,
        purpose: ReadPurpose,
        legacy_enabled: bool,
        current_directory: &Path,
    ) -> io::Result<ReadCandidates> {
        let absolute = normalize(requested, current_directory, native_platform())?;
        let mut result = ReadCandidates {
            requested: requested.to_path_buf(),
            primary: absolute.clone(),
            fallback: None,
            origin: ReadOrigin::Explicit,
        };
        if !matches!(
            purpose,
            ReadPurpose::ApplicationState | ReadPurpose::ApplicationData
        ) {
            return Ok(result);
        }
        // XDG state/data/config homes may coincide. Known configuration and
        // credential addresses still never acquire compatibility lookup.
        for paths in [self.current(), self.legacy()] {
            for excluded in [
                paths.config_file(),
                paths.predefined_config_file(),
                paths.credentials_file(),
                paths.credential_preset_directory(),
            ] {
                if absolute.starts_with(normalize(&excluded, current_directory, native_platform())?)
                {
                    return Ok(result);
                }
            }
        }
        let eligible = allows_legacy_fallback(legacy_enabled, purpose, PrimaryReadFailure::Absent);
        let mut pairs = [
            (self.current().state(), self.legacy().state()),
            (self.current().data(), self.legacy().data()),
        ]
        .into_iter()
        .map(|(current, legacy)| {
            Ok((
                normalize(current, current_directory, native_platform())?,
                normalize(legacy, current_directory, native_platform())?,
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
        pairs.sort_by_key(|(current, _)| std::cmp::Reverse(current.components().count()));
        for (current, legacy) in pairs {
            if let Ok(relative) = absolute.strip_prefix(&current) {
                result.origin = ReadOrigin::Primary;
                if eligible {
                    result.fallback = Some(legacy.join(relative));
                }
                break;
            }
            if eligible {
                if let Ok(relative) = absolute.strip_prefix(&legacy) {
                    result.primary = current.join(relative);
                    result.fallback = Some(absolute.clone());
                    result.origin = ReadOrigin::Primary;
                    break;
                }
            }
        }
        Ok(result)
    }
}

impl ReadCandidates {
    pub fn requested_path(&self) -> &Path {
        &self.requested
    }
    pub fn primary_path(&self) -> &Path {
        &self.primary
    }
    pub fn fallback_path(&self) -> Option<&Path> {
        self.fallback.as_deref()
    }
    pub fn origin(&self) -> ReadOrigin {
        self.origin
    }

    /// The callback must authorize and perform the real read against the actual
    /// target, including its operation-level symlink/race checks (T36). This
    /// canonicalized report is not an authorization token. Only absence retries;
    /// denied, malformed, canceled and other outcomes are returned unchanged.
    pub fn read_with<T>(
        &self,
        mut read: impl FnMut(&ResolvedRead) -> io::Result<T>,
    ) -> io::Result<ReadValue<T>> {
        let attempt = |candidate: &Path,
                       origin: ReadOrigin,
                       read: &mut dyn FnMut(&ResolvedRead) -> io::Result<T>|
         -> io::Result<ReadValue<T>> {
            let actual = fs::canonicalize(candidate)?;
            let resolution = ResolvedRead::new(self.requested.clone(), actual, origin);
            let value = read(&resolution)?;
            Ok(ReadValue { resolution, value })
        };
        match attempt(&self.primary, self.origin, &mut read) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(fallback) = &self.fallback {
                    attempt(fallback, ReadOrigin::LegacyFallback, &mut read)
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
}
