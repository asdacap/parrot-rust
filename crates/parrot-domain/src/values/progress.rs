use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROGRESS_VERSION: u64 = 1;
pub const PROGRESS_FILE_NAME: &str = "progress.json";
pub const PROGRESS_STAGING_FILE_NAME: &str = "progress.json.tmp";
pub const PROGRESS_INVENTORY_KIND: &str = "agent_progress";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub steps: u64,
    pub total_steps: u64,
    pub current_step: Option<String>,
}
impl Progress {
    pub fn validate(&self) -> Result<(), ProgressValidationError> {
        if self.total_steps == 0 {
            return Err(ProgressValidationError::ZeroTotal);
        }
        if self.steps > self.total_steps {
            return Err(ProgressValidationError::StepsExceedTotal);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressUpdate {
    Replace(Progress),
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressSnapshot {
    pub schema_version: u64,
    pub owner_session_id: String,
    pub revision: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub progress: Option<Progress>,
}
impl ProgressSnapshot {
    pub fn new(owner_session_id: String) -> Self {
        Self {
            schema_version: PROGRESS_VERSION,
            owner_session_id,
            revision: 0,
            progress: None,
        }
    }

    pub fn validate(&self) -> Result<(), ProgressValidationError> {
        if self.schema_version != PROGRESS_VERSION {
            return Err(ProgressValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        if self.owner_session_id.trim().is_empty() {
            return Err(ProgressValidationError::EmptyOwner);
        }
        if let Some(progress) = &self.progress {
            progress.validate()?;
        }
        Ok(())
    }

    /// Applies a replacement or explicit close, returning false for an already
    /// closed report. Validation and revision overflow leave this value unchanged.
    pub fn apply(&mut self, update: ProgressUpdate) -> Result<bool, ProgressValidationError> {
        self.validate()?;
        let progress = match update {
            ProgressUpdate::Replace(progress) => {
                progress.validate()?;
                Some(progress)
            }
            ProgressUpdate::Close if self.progress.is_none() => return Ok(false),
            ProgressUpdate::Close => None,
        };
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(ProgressValidationError::RevisionOverflow)?;
        self.revision = revision;
        self.progress = progress;
        Ok(true)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressValidationError {
    UnsupportedVersion(u64),
    EmptyOwner,
    ZeroTotal,
    StepsExceedTotal,
    RevisionOverflow,
}
impl fmt::Display for ProgressValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported progress snapshot version {version}")
            }
            Self::EmptyOwner => formatter.write_str("progress snapshot owner must not be blank"),
            Self::ZeroTotal => formatter.write_str("total_steps must be positive"),
            Self::StepsExceedTotal => formatter.write_str("steps must not exceed total_steps"),
            Self::RevisionOverflow => formatter.write_str("progress snapshot revision overflow"),
        }
    }
}
impl std::error::Error for ProgressValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_replanning_close_and_reopen_increment_revision() {
        let mut snapshot = ProgressSnapshot::new("owner".into());
        assert!(!snapshot.apply(ProgressUpdate::Close).unwrap());
        for progress in [
            Progress {
                steps: 5,
                total_steps: 5,
                current_step: Some("Complete but still open".into()),
            },
            Progress {
                steps: 1,
                total_steps: 3,
                current_step: None,
            },
        ] {
            assert!(
                snapshot
                    .apply(ProgressUpdate::Replace(progress.clone()))
                    .unwrap()
            );
            assert_eq!(snapshot.progress.as_ref(), Some(&progress));
        }
        assert_eq!(snapshot.revision, 2);
        assert!(snapshot.apply(ProgressUpdate::Close).unwrap());
        assert_eq!(snapshot.progress, None);
        assert_eq!(snapshot.revision, 3);
        assert!(!snapshot.apply(ProgressUpdate::Close).unwrap());
        assert_eq!(snapshot.revision, 3);
        assert!(
            snapshot
                .apply(ProgressUpdate::Replace(Progress {
                    steps: 0,
                    total_steps: u64::MAX,
                    current_step: None,
                }))
                .unwrap()
        );
        assert_eq!(snapshot.revision, 4);
    }

    #[test]
    fn invalid_updates_and_revision_overflow_leave_snapshot_unchanged() {
        let mut snapshot = ProgressSnapshot::new("owner".into());
        for (steps, total_steps, error) in [
            (0, 0, ProgressValidationError::ZeroTotal),
            (2, 1, ProgressValidationError::StepsExceedTotal),
        ] {
            let before = snapshot.clone();
            assert_eq!(
                snapshot.apply(ProgressUpdate::Replace(Progress {
                    steps,
                    total_steps,
                    current_step: None,
                })),
                Err(error)
            );
            assert_eq!(snapshot, before);
        }
        snapshot.revision = u64::MAX;
        let before = snapshot.clone();
        assert_eq!(
            snapshot.apply(ProgressUpdate::Replace(Progress {
                steps: u64::MAX,
                total_steps: u64::MAX,
                current_step: None,
            })),
            Err(ProgressValidationError::RevisionOverflow)
        );
        assert_eq!(snapshot, before);
    }
}
