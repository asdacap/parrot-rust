use super::{
    ActivationStatus, AdmissionResult, ClaimDisposition, OpenIntent, OpenOperation,
    RuntimeIdentity, WorkingDirectoryClaim,
};
use crate::history_files::{SessionCatalog, SessionCatalogEntry, SessionCatalogState};
use parrot_domain::contracts::paths::{RootOrigin, SelectedSessionRoot};
use parrot_domain::values::identifiers::UserSessionId;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Both paths must come from one current HOME/XDG resolution. The application
/// component alone differs; discovery never searches earlier homes.
#[derive(Clone, Debug)]
pub struct OwnershipRoots {
    current: PathBuf,
    legacy: PathBuf,
}
impl OwnershipRoots {
    pub fn new(current: PathBuf, legacy: PathBuf) -> io::Result<Self> {
        for path in [&current, &legacy] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "state roots must be absolute",
                ));
            }
        }
        Ok(Self { current, legacy })
    }
    pub fn current(&self) -> &Path {
        &self.current
    }
    pub fn legacy(&self) -> &Path {
        &self.legacy
    }
}

pub struct SessionAdmission {
    pub selected_root: Option<SelectedSessionRoot>,
    pub admission: AdmissionResult,
}

/// Ownership safety always checks both supplied roots. The fallback flag only
/// changes future discovery/resume; returned roots and leases are immutable.
pub struct SessionAdmissions {
    roots: OwnershipRoots,
    current: WorkingDirectoryClaim,
    legacy: WorkingDirectoryClaim,
    legacy_enabled: AtomicBool,
}
impl SessionAdmissions {
    pub fn new(roots: OwnershipRoots, runtime: RuntimeIdentity, legacy_enabled: bool) -> Self {
        Self {
            current: WorkingDirectoryClaim::new(roots.current.clone(), runtime.clone()),
            legacy: WorkingDirectoryClaim::new(roots.legacy.clone(), runtime),
            roots,
            legacy_enabled: AtomicBool::new(legacy_enabled),
        }
    }
    pub fn roots(&self) -> &OwnershipRoots {
        &self.roots
    }
    pub fn set_legacy_path_read_fallback(&self, enabled: bool) {
        self.legacy_enabled.store(enabled, Ordering::Release);
    }
    pub fn legacy_path_read_fallback(&self) -> bool {
        self.legacy_enabled.load(Ordering::Acquire)
    }

    pub fn discover_latest(&self, workspace: &Path) -> io::Result<SessionAdmission> {
        let canonical = WorkingDirectoryClaim::canonicalize(workspace)?;
        let Some(current) = self.current.workspace_associations(&canonical)? else {
            return Ok(unselected(ClaimDisposition::Corrupt));
        };
        let legacy_enabled = self.legacy_path_read_fallback();
        let legacy = if legacy_enabled {
            let Some(ids) = self.legacy.workspace_associations(&canonical)? else {
                return Ok(unselected(ClaimDisposition::Corrupt));
            };
            ids
        } else {
            Vec::new()
        };
        let mut candidates = BTreeMap::new();
        for id in current.iter().chain(legacy.iter()) {
            if candidates.contains_key(id.value()) {
                continue;
            }
            let current_path = self.roots.current.join("sessions").join(id.value());
            let current_exists = directory_exists(&current_path)?;
            let legacy_path = self.roots.legacy.join("sessions").join(id.value());
            let origin = if current_exists {
                RootOrigin::Current
            } else if legacy_enabled && directory_exists(&legacy_path)? {
                RootOrigin::Legacy
            } else if current.contains(id) {
                RootOrigin::Current
            } else {
                RootOrigin::Legacy
            };
            let claim = self.claim(origin);
            let (status, corrupt) = claim.activation_status(id, &canonical)?;
            if corrupt {
                return Ok(unselected(ClaimDisposition::Corrupt));
            }
            let catalog = SessionCatalog::new(claim.state_directory());
            let metadata = catalog.find(id);
            let recency = match metadata {
                Some(entry) if entry.state == SessionCatalogState::Corrupt => {
                    let root = claim.state_directory().join("sessions").join(id.value());
                    if fs::symlink_metadata(&root)?.file_type().is_symlink()
                        || root.join("meta.json").is_file()
                    {
                        return Ok(unselected(ClaimDisposition::Corrupt));
                    }
                    String::new()
                }
                Some(entry) => {
                    if WorkingDirectoryClaim::canonicalize(Path::new(&entry.working_directory))?
                        != canonical
                    {
                        return Ok(unselected(ClaimDisposition::Corrupt));
                    }
                    entry
                        .last_opened_at
                        .filter(|date| !date.is_empty())
                        .or(entry.created_at)
                        .unwrap_or_default()
                }
                None => String::new(),
            };
            candidates.insert(id.value().to_owned(), (id.clone(), origin, recency, status));
        }
        let mut ordered: Vec<_> = candidates.into_values().collect();
        ordered.sort_by(|a, b| {
            b.2.encode_utf16()
                .cmp(a.2.encode_utf16())
                .then_with(|| a.0.value().encode_utf16().cmp(b.0.value().encode_utf16()))
        });
        let Some((id, origin, _, status)) = ordered.into_iter().next() else {
            return Ok(unselected(ClaimDisposition::Fresh));
        };
        let disposition = if status == ActivationStatus::Inactive {
            ClaimDisposition::Resumed
        } else {
            ClaimDisposition::Live
        };
        Ok(SessionAdmission {
            selected_root: Some(self.selected_root(&id, origin)?),
            admission: AdmissionResult {
                disposition,
                session_id: Some(id.clone()),
                activation_lease: None,
                intent: Some(OpenIntent {
                    operation: OpenOperation::Resume,
                    session_id: id,
                    working_directory: workspace.to_string_lossy().into_owned(),
                    canonical_workspace_identity: canonical,
                }),
            },
        })
    }

    pub fn open_default(&self, workspace: &Path) -> io::Result<SessionAdmission> {
        let discovered = self.discover_latest(workspace)?;
        match (
            discovered.admission.disposition,
            discovered.admission.session_id.as_ref(),
        ) {
            (ClaimDisposition::Fresh, _) => {
                self.create_fresh(workspace, &UserSessionId::generate()?)
            }
            (ClaimDisposition::Resumed, Some(id)) => self.resume(workspace, id, false),
            _ => Ok(discovered),
        }
    }

    pub fn create_fresh(
        &self,
        workspace: &Path,
        id: &UserSessionId,
    ) -> io::Result<SessionAdmission> {
        let root = self.selected_root(id, RootOrigin::Current)?;
        if let Some(disposition) = self.safety(id, workspace, false)? {
            return Ok(refused(root, disposition, id));
        }
        let claim = self.guarded_claim(RootOrigin::Current);
        Ok(SessionAdmission {
            selected_root: Some(root),
            admission: claim.create_fresh(workspace, id)?,
        })
    }

    pub fn resume(
        &self,
        workspace: &Path,
        id: &UserSessionId,
        take_over: bool,
    ) -> io::Result<SessionAdmission> {
        let origin = self.resume_origin(id)?;
        let root = self.selected_root(id, origin)?;
        if let Some(disposition) = self.safety(id, workspace, take_over)? {
            return Ok(refused(root, disposition, id));
        }
        let claim = self.guarded_claim(origin);
        Ok(SessionAdmission {
            selected_root: Some(root),
            admission: claim.resume(workspace, id, take_over)?,
        })
    }

    /// Read metadata from the same eligible root that a resume would select.
    pub fn resume_metadata(&self, id: &UserSessionId) -> io::Result<Option<SessionCatalogEntry>> {
        let origin = self.resume_origin(id)?;
        Ok(SessionCatalog::new(self.claim(origin).state_directory()).find(id))
    }

    fn resume_origin(&self, id: &UserSessionId) -> io::Result<RootOrigin> {
        let current = self.roots.current.join("sessions").join(id.value());
        if directory_exists(&current)? || !self.legacy_path_read_fallback() {
            Ok(RootOrigin::Current)
        } else if directory_exists(&self.roots.legacy.join("sessions").join(id.value()))? {
            Ok(RootOrigin::Legacy)
        } else {
            Ok(RootOrigin::Current)
        }
    }

    fn claim(&self, origin: RootOrigin) -> &WorkingDirectoryClaim {
        match origin {
            RootOrigin::Current => &self.current,
            RootOrigin::Legacy => &self.legacy,
        }
    }
    fn safety(
        &self,
        id: &UserSessionId,
        workspace: &Path,
        take_over: bool,
    ) -> io::Result<Option<ClaimDisposition>> {
        let canonical = WorkingDirectoryClaim::canonicalize(workspace)?;
        safety(&self.current, &self.legacy, id, &canonical, take_over)
    }
    fn guarded_claim(&self, origin: RootOrigin) -> WorkingDirectoryClaim {
        let current = self.current.clone();
        let legacy = self.legacy.clone();
        self.claim(origin)
            .clone()
            .with_activation_guard(Arc::new(move |intent, take_over| {
                safety(
                    &current,
                    &legacy,
                    &intent.session_id,
                    &intent.canonical_workspace_identity,
                    take_over,
                )
            }))
    }
    fn selected_root(
        &self,
        id: &UserSessionId,
        origin: RootOrigin,
    ) -> io::Result<SelectedSessionRoot> {
        let root = self
            .claim(origin)
            .state_directory()
            .join("sessions")
            .join(id.value());
        directory_exists(&root)?;
        SelectedSessionRoot::new(materialize(&root)?, origin)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    }
}

fn safety(
    current: &WorkingDirectoryClaim,
    legacy: &WorkingDirectoryClaim,
    id: &UserSessionId,
    canonical: &str,
    take_over: bool,
) -> io::Result<Option<ClaimDisposition>> {
    for claim in [current, legacy] {
        let (status, corrupt) = claim.admission_safety_status(id, canonical)?;
        if corrupt {
            return Ok(Some(ClaimDisposition::Corrupt));
        }
        if status.prevents_acquisition(take_over) {
            return Ok(Some(ClaimDisposition::Live));
        }
    }
    Ok(None)
}

fn unselected(disposition: ClaimDisposition) -> SessionAdmission {
    SessionAdmission {
        selected_root: None,
        admission: AdmissionResult {
            disposition,
            session_id: None,
            activation_lease: None,
            intent: None,
        },
    }
}
fn refused(
    root: SelectedSessionRoot,
    disposition: ClaimDisposition,
    id: &UserSessionId,
) -> SessionAdmission {
    SessionAdmission {
        selected_root: Some(root),
        admission: AdmissionResult {
            disposition,
            session_id: Some(id.clone()),
            activation_lease: None,
            intent: None,
        },
    }
}
fn directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session root must be a directory, not a symbolic link or file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
fn materialize(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    let mut physical = loop {
        match fs::canonicalize(existing) {
            Ok(path) => break path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(existing).is_ok_and(|meta| meta.file_type().is_symlink()) {
                    return Err(error);
                }
                missing.push(
                    existing
                        .file_name()
                        .ok_or_else(|| io::Error::other("session root has no ancestor"))?
                        .to_os_string(),
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| io::Error::other("session root has no parent"))?;
            }
            Err(error) => return Err(error),
        }
    };
    for part in missing.into_iter().rev() {
        physical.push(part);
    }
    Ok(physical)
}
