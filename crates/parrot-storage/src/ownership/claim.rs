use super::runtime::{fingerprint_host, random_id};
use super::types::*;
use crate::history_files::json::HistoryJsonFormatter;
use parrot_domain::values::identifiers::UserSessionId;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
};

#[derive(Clone)]
pub struct WorkingDirectoryClaim {
    state_directory: PathBuf,
    runtime: RuntimeIdentity,
    activation_guard: Option<ActivationGuard>,
}
impl WorkingDirectoryClaim {
    pub fn new(state_directory: impl Into<PathBuf>, runtime: RuntimeIdentity) -> Self {
        Self {
            state_directory: state_directory.into(),
            runtime,
            activation_guard: None,
        }
    }
    pub fn with_activation_guard(mut self, guard: ActivationGuard) -> Self {
        self.activation_guard = Some(guard);
        self
    }
    pub fn workspace_associations(
        &self,
        canonical: &str,
    ) -> io::Result<Option<Vec<UserSessionId>>> {
        self.read_associations(canonical)
    }
    pub fn admission_safety_status(
        &self,
        id: &UserSessionId,
        canonical: &str,
    ) -> io::Result<(ActivationStatus, bool)> {
        if let Some(head) = Self::current(&self.activation_directory(id))? {
            return Ok(if matches_association(Some(&head), id, canonical) {
                (self.determine_status(Some(&head)), false)
            } else {
                (ActivationStatus::Uncertain, true)
            });
        }
        Ok(match self.read_legacy(canonical)? {
            None => (ActivationStatus::Uncertain, true),
            Some(legacy) => (self.determine_status(legacy.as_ref()), false),
        })
    }
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }
    pub fn owners_directory(&self) -> PathBuf {
        self.state_directory.join("owners")
    }
    pub fn fingerprint(value: &str) -> String {
        format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_owned()
    }
    pub fn canonicalize(path: &Path) -> io::Result<String> {
        if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Empty working directory",
            ));
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut normalized = PathBuf::new();
        for part in absolute.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                _ => normalized.push(part.as_os_str()),
            }
        }
        #[cfg(target_os = "macos")]
        {
            for root in ["/var", "/tmp", "/etc"] {
                if normalized.starts_with(root) {
                    normalized = Path::new("/private").join(normalized.strip_prefix("/").unwrap());
                    break;
                }
            }
        }
        // Resolve a final directory link, without resolving ordinary ancestor links.
        match fs::symlink_metadata(&normalized) {
            Ok(meta) if meta.file_type().is_symlink() => {
                normalized = fs::canonicalize(&normalized)?
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        normalized
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Non-UTF8 working directory"))
    }
    pub fn current(directory: &Path) -> io::Result<Option<OwnerRecord>> {
        let Some(files) = entries(directory)? else {
            return Ok(None);
        };
        let mut newest = None;
        let mut newest_version = 0;
        for file in files {
            let file = file?;
            if file.path().is_dir() || !matches_name(&file.path(), "v") {
                continue;
            }
            if let Some(record) = read(&file.path())?
                && record
                    .version
                    .is_some_and(|version| version > newest_version)
            {
                newest_version = record.version.unwrap();
                newest = Some(record);
            }
        }
        Ok(newest)
    }
    pub fn activation_status(
        &self,
        id: &UserSessionId,
        canonical: &str,
    ) -> io::Result<(ActivationStatus, bool)> {
        Ok(match self.read_activation(id, canonical)? {
            Some(status) => (status, false),
            None => (ActivationStatus::Uncertain, true),
        })
    }
    pub fn is_active(&self, id: &UserSessionId, working_directory: &Path) -> io::Result<bool> {
        Ok(matches!(
            self.read_activation(id, &Self::canonicalize(working_directory)?)?,
            Some(ActivationStatus::LiveOwnerNotDisproved)
        ))
    }
    pub fn create_fresh(
        &self,
        working_directory: &Path,
        id: &UserSessionId,
    ) -> io::Result<AdmissionResult> {
        let canonical = Self::canonicalize(working_directory)?;
        let intent = intent(
            OpenOperation::CreateFresh,
            working_directory,
            id,
            canonical.clone(),
        );
        let association = OwnerRecord {
            schema_version: Some(2),
            user_session_id: Some(id.value().into()),
            session_id: Some(id.value().into()),
            working_directory: Some(intent.working_directory.clone()),
            canonical_workspace_identity: Some(canonical.clone()),
            ..Default::default()
        };
        let directory = self.workspace_directory(&canonical);
        let path = directory.join(format!("{}.json", id.value()));
        if path.try_exists()? && !matches_association(read(&path)?.as_ref(), id, &canonical) {
            return Ok(result(
                ClaimDisposition::Corrupt,
                Some(id.clone()),
                Some(intent),
            ));
        }
        let admitted = self.activate(intent.clone(), ClaimDisposition::Fresh, false)?;
        if admitted.activation_lease.is_some() {
            // Refusing guards must not publish an association. On a later
            // publication failure, dropping admitted fences/releases its lease.
            fs::create_dir_all(&directory)?;
            if !write_once(&path, &association)?
                && !matches_association(read(&path)?.as_ref(), id, &canonical)
            {
                return Ok(result(
                    ClaimDisposition::Corrupt,
                    Some(id.clone()),
                    Some(intent),
                ));
            }
        }
        Ok(admitted)
    }
    pub fn resume(
        &self,
        working_directory: &Path,
        id: &UserSessionId,
        take_over: bool,
    ) -> io::Result<AdmissionResult> {
        let canonical = Self::canonicalize(working_directory)?;
        let intent = intent(
            OpenOperation::Resume,
            working_directory,
            id,
            canonical.clone(),
        );
        let path = self
            .workspace_directory(&canonical)
            .join(format!("{}.json", id.value()));
        let valid = if path.is_file() {
            matches_association(read(&path)?.as_ref(), id, &canonical)
        } else {
            self.read_legacy(&canonical)?.is_some_and(|record| {
                record.is_some_and(|record| session_id(&record).as_ref() == Some(id))
            })
        };
        if !valid {
            return Ok(result(
                ClaimDisposition::Corrupt,
                Some(id.clone()),
                Some(intent),
            ));
        }
        self.activate(intent, ClaimDisposition::Resumed, take_over)
    }
    pub(crate) fn release(&self, lease: &SessionActivationLease) -> io::Result<()> {
        let directory = self.activation_directory(&lease.intent.session_id);
        if let Some(mut head) = Self::current(&directory)?
            && head.generation == Some(lease.generation)
            && head.runtime_instance_id.as_deref() == Some(&lease.runtime_instance_id)
            && head.lease_id.as_deref() == Some(&lease.lease_id)
            && is_state(&head, "Active")
        {
            head.version = Some(head.version.unwrap_or(0).wrapping_add(1));
            head.state = Some("Inactive".into());
            let _ = write_version(&directory, &head)?;
        }
        Ok(())
    }
    pub fn determine_status(&self, record: Option<&OwnerRecord>) -> ActivationStatus {
        let Some(record) = record else {
            return ActivationStatus::Inactive;
        };
        if is_state(record, "Inactive") {
            return ActivationStatus::Inactive;
        }
        let unknown_state = record.state.is_some() && !is_state(record, "Active");
        let host = record
            .host_identity
            .clone()
            .unwrap_or_else(|| fingerprint_host(record.host_key.as_deref().unwrap_or_default()));
        if host != self.runtime.host_identity {
            return ActivationStatus::Uncertain;
        }
        let Some(pid) = record.process_id else {
            return ActivationStatus::Uncertain;
        };
        let process = (self.runtime.inspect)(pid);
        match process.status {
            ProcessIdentityStatus::Found => ActivationStatus::LiveOwnerNotDisproved,
            ProcessIdentityStatus::Missing if !unknown_state => ActivationStatus::Inactive,
            _ => ActivationStatus::Uncertain,
        }
    }
    fn activate(
        &self,
        intent: OpenIntent,
        success: ClaimDisposition,
        take_over: bool,
    ) -> io::Result<AdmissionResult> {
        let directory = self.activation_directory(&intent.session_id);
        for _ in 0..32 {
            if let Some(guard) = &self.activation_guard
                && let Some(disposition) = guard(&intent, take_over)?
            {
                return Ok(result(
                    disposition,
                    Some(intent.session_id.clone()),
                    Some(intent),
                ));
            }
            let head = Self::current(&directory)?;
            let status = if head.is_some() {
                self.determine_status(head.as_ref())
            } else {
                let Some(status) =
                    self.read_activation(&intent.session_id, &intent.canonical_workspace_identity)?
                else {
                    return Ok(result(
                        ClaimDisposition::Corrupt,
                        Some(intent.session_id.clone()),
                        Some(intent),
                    ));
                };
                status
            };
            if status.prevents_acquisition(take_over) {
                return Ok(result(
                    ClaimDisposition::Live,
                    Some(intent.session_id.clone()),
                    Some(intent),
                ));
            }
            let lease_id = random_id()?;
            let generation = head
                .as_ref()
                .and_then(|head| head.generation)
                .unwrap_or(0)
                .wrapping_add(1);
            let version = head
                .as_ref()
                .and_then(|head| head.version)
                .unwrap_or(0)
                .wrapping_add(1);
            fs::create_dir_all(&directory)?;
            if !write_version(
                &directory,
                &self.active_record(&intent, version, generation, lease_id.clone()),
            )? {
                continue;
            }
            let lease = SessionActivationLease::new(
                self.clone(),
                intent.clone(),
                lease_id,
                generation,
                self.runtime.runtime_instance_id.clone(),
            );
            return Ok(AdmissionResult {
                disposition: success,
                session_id: Some(intent.session_id.clone()),
                activation_lease: Some(lease),
                intent: Some(intent),
            });
        }
        Ok(result(
            ClaimDisposition::Contended,
            Some(intent.session_id.clone()),
            Some(intent),
        ))
    }
    fn active_record(
        &self,
        intent: &OpenIntent,
        version: i32,
        generation: i64,
        lease_id: String,
    ) -> OwnerRecord {
        OwnerRecord {
            schema_version: Some(2),
            version: Some(version),
            generation: Some(generation),
            state: Some("Active".into()),
            user_session_id: Some(intent.session_id.value().into()),
            session_id: Some(intent.session_id.value().into()),
            working_directory: Some(intent.working_directory.clone()),
            canonical_workspace_identity: Some(intent.canonical_workspace_identity.clone()),
            host_identity: Some(self.runtime.host_identity.clone()),
            boot_identity: Some(self.runtime.boot_identity.clone()),
            process_id: Some(self.runtime.process_id),
            process_start_token: Some(self.runtime.process_start_token.clone()),
            runtime_instance_id: Some(self.runtime.runtime_instance_id.clone()),
            lease_id: Some(lease_id),
            ..Default::default()
        }
    }
    fn read_associations(&self, canonical: &str) -> io::Result<Option<Vec<UserSessionId>>> {
        let mut result = BTreeMap::new();
        if let Some(files) = entries(&self.workspace_directory(canonical))? {
            for file in files {
                let path = file?.path();
                if path.is_dir() || !matches_name(&path, "") {
                    continue;
                }
                let record = read(&path)?;
                let Some(id) = record.as_ref().and_then(session_id) else {
                    return Ok(None);
                };
                if !matches_association(record.as_ref(), &id, canonical)
                    || path.file_stem().and_then(|s| s.to_str()) != Some(id.value())
                {
                    return Ok(None);
                }
                result.insert(id.value().to_owned(), id);
            }
        }
        let Some(legacy) = self.read_legacy(canonical)? else {
            return Ok(None);
        };
        if let Some(id) = legacy.as_ref().and_then(session_id) {
            result.entry(id.value().into()).or_insert(id);
        }
        Ok(Some(result.into_values().collect()))
    }
    // Outer None means corruption; inner None means no legacy owner.
    fn read_legacy(&self, canonical: &str) -> io::Result<Option<Option<OwnerRecord>>> {
        let Some(record) =
            Self::current(&self.owners_directory().join(Self::fingerprint(canonical)))?
        else {
            return Ok(Some(None));
        };
        if session_id(&record).is_none() {
            return Ok(None);
        }
        let recorded = match &record.canonical_workspace_identity {
            Some(value) => value.clone(),
            None => match Self::canonicalize(Path::new(
                record.working_directory.as_deref().unwrap_or_default(),
            )) {
                Ok(value) => value,
                Err(_) => return Ok(None),
            },
        };
        Ok((recorded == canonical).then_some(Some(record)))
    }
    fn read_activation(
        &self,
        id: &UserSessionId,
        canonical: &str,
    ) -> io::Result<Option<ActivationStatus>> {
        if let Some(head) = Self::current(&self.activation_directory(id))? {
            return Ok(matches_association(Some(&head), id, canonical)
                .then(|| self.determine_status(Some(&head))));
        }
        let Some(legacy) = self.read_legacy(canonical)? else {
            return Ok(None);
        };
        Ok(Some(
            if legacy.as_ref().and_then(session_id).as_ref() == Some(id) {
                self.determine_status(legacy.as_ref())
            } else {
                ActivationStatus::Inactive
            },
        ))
    }
    fn workspace_directory(&self, canonical: &str) -> PathBuf {
        self.owners_directory()
            .join("workspaces")
            .join(Self::fingerprint(canonical))
    }
    fn activation_directory(&self, id: &UserSessionId) -> PathBuf {
        self.owners_directory().join("activations").join(id.value())
    }
}
fn result(
    disposition: ClaimDisposition,
    session_id: Option<UserSessionId>,
    intent: Option<OpenIntent>,
) -> AdmissionResult {
    AdmissionResult {
        disposition,
        session_id,
        activation_lease: None,
        intent,
    }
}
fn intent(
    operation: OpenOperation,
    path: &Path,
    id: &UserSessionId,
    canonical: String,
) -> OpenIntent {
    OpenIntent {
        operation,
        session_id: id.clone(),
        working_directory: path.to_string_lossy().into_owned(),
        canonical_workspace_identity: canonical,
    }
}
fn session_id(record: &OwnerRecord) -> Option<UserSessionId> {
    record
        .user_session_id
        .as_ref()
        .or(record.session_id.as_ref())
        .and_then(|id| UserSessionId::try_parse(id))
}
fn matches_association(record: Option<&OwnerRecord>, id: &UserSessionId, canonical: &str) -> bool {
    record.is_some_and(|record| {
        session_id(record).as_ref() == Some(id)
            && record.canonical_workspace_identity.as_deref() == Some(canonical)
    })
}
fn is_state(record: &OwnerRecord, state: &str) -> bool {
    record
        .state
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(state))
}
fn matches_name(path: &Path, prefix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".json"))
}
fn entries(path: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::NotADirectory =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
fn read(path: &Path) -> io::Result<Option<OwnerRecord>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_str(&decode_owner_text(&bytes)).ok()),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(error),
        Err(_) => Ok(None),
    }
}
fn decode_owner_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    // File.ReadAllText detects UTF-32 before UTF-16 because their LE BOMs overlap.
    let utf32 = bytes
        .strip_prefix(b"\xff\xfe\x00\x00")
        .map(|body| (body, true))
        .or_else(|| {
            bytes
                .strip_prefix(b"\x00\x00\xfe\xff")
                .map(|body| (body, false))
        });
    if let Some((body, little_endian)) = utf32 {
        let (chunks, remainder) = body.as_chunks::<4>();
        let mut text: String = chunks
            .iter()
            .map(|chunk| {
                let code = if little_endian {
                    u32::from_le_bytes(*chunk)
                } else {
                    u32::from_be_bytes(*chunk)
                };
                char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
            .collect();
        if !remainder.is_empty() {
            text.push(char::REPLACEMENT_CHARACTER);
        }
        return text.into();
    }
    let utf16 = bytes
        .strip_prefix(b"\xff\xfe")
        .map(|body| (body, true))
        .or_else(|| bytes.strip_prefix(b"\xfe\xff").map(|body| (body, false)));
    if let Some((body, little_endian)) = utf16 {
        let (chunks, remainder) = body.as_chunks::<2>();
        let units: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                if little_endian {
                    u16::from_le_bytes(*chunk)
                } else {
                    u16::from_be_bytes(*chunk)
                }
            })
            .collect();
        let mut text = String::from_utf16_lossy(&units);
        if !remainder.is_empty() {
            text.push(char::REPLACEMENT_CHARACTER);
        }
        return text.into();
    }
    String::from_utf8_lossy(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes))
}

fn write_version(directory: &Path, record: &OwnerRecord) -> io::Result<bool> {
    let version = record.version.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Ledger record needs a version")
    })?;
    write_once(&directory.join(format!("v{version}.json")), record)
}
fn write_once(target: &Path, record: &OwnerRecord) -> io::Result<bool> {
    let staged = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Owner path needs a directory"))?
        .join(format!(".staging-{}", random_id()?));
    fs::write(&staged, serialize_owner(record)?)?;
    let linked = fs::hard_link(&staged, target).is_ok();
    fs::remove_file(staged)?;
    Ok(linked)
}
pub fn serialize_owner(record: &OwnerRecord) -> io::Result<Vec<u8>> {
    use serde::Serialize;
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut bytes,
        HistoryJsonFormatter(serde_json::ser::PrettyFormatter::with_indent(b"  ")),
    );
    record
        .serialize(&mut serializer)
        .map_err(io::Error::other)?;
    Ok(bytes)
}
