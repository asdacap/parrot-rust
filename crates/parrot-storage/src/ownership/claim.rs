use super::runtime::{fingerprint_host, random_id};
use super::types::*;
use crate::history_files::{SessionCatalog, SessionCatalogState};
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
            if let Some(record) = read(&file.path())? {
                if record
                    .version
                    .is_some_and(|version| version > newest_version)
                {
                    newest_version = record.version.unwrap();
                    newest = Some(record);
                }
            }
        }
        Ok(newest)
    }
    pub fn discover_latest(&self, working_directory: &Path) -> io::Result<AdmissionResult> {
        self.discover(working_directory, true)
    }
    fn discover(
        &self,
        working_directory: &Path,
        recheck_inactive: bool,
    ) -> io::Result<AdmissionResult> {
        let canonical = Self::canonicalize(working_directory)?;
        let Some(associations) = self.read_associations(&canonical)? else {
            return Ok(result(ClaimDisposition::Corrupt, None, None));
        };
        let catalog = SessionCatalog::new(&self.state_directory);
        let mut candidates = Vec::new();
        for id in associations {
            let Some(status) = self.read_activation(&id, &canonical)? else {
                return Ok(result(ClaimDisposition::Corrupt, None, None));
            };
            let mut metadata = catalog.find(&id);
            let session_directory = self.state_directory.join("sessions").join(id.value());
            if metadata
                .as_ref()
                .is_some_and(|meta| meta.state == SessionCatalogState::Corrupt)
                && !fs::symlink_metadata(&session_directory)?
                    .file_type()
                    .is_symlink()
                && !session_directory.join("meta.json").is_file()
            {
                metadata = None;
            }
            if let Some(meta) = &metadata {
                if meta.state == SessionCatalogState::Corrupt
                    || Self::canonicalize(Path::new(&meta.working_directory))? != canonical
                {
                    return Ok(result(ClaimDisposition::Corrupt, None, None));
                }
            }
            let recency = metadata
                .map(|meta| {
                    meta.last_opened_at
                        .filter(|date| !date.is_empty())
                        .or(meta.created_at)
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            candidates.push((id, recency, status));
        }
        candidates.sort_by(|(a_id, a_date, _), (b_id, b_date, _)| {
            ordinal_cmp(b_date, a_date).then_with(|| ordinal_cmp(a_id.value(), b_id.value()))
        });
        let Some((id, _, status)) = candidates.into_iter().next() else {
            return Ok(result(ClaimDisposition::Fresh, None, None));
        };
        // OpenDefault's inactive candidate is rechecked by activation before
        // any write. Standalone discovery still rechecks before returning.
        let status = if !recheck_inactive && status == ActivationStatus::Inactive {
            Some(status)
        } else {
            self.read_activation(&id, &canonical)?
        };
        let disposition = match status {
            None => ClaimDisposition::Corrupt,
            Some(ActivationStatus::Inactive) => ClaimDisposition::Resumed,
            _ => ClaimDisposition::Live,
        };
        let intent = intent(OpenOperation::Resume, working_directory, &id, canonical);
        Ok(result(disposition, Some(id), Some(intent)))
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
            Some(ActivationStatus::Active | ActivationStatus::LiveOwnerNotDisproved)
        ))
    }
    pub fn open_default(&self, working_directory: &Path) -> io::Result<AdmissionResult> {
        let candidate = self.discover(working_directory, false)?;
        match (&candidate.disposition, &candidate.session_id) {
            (ClaimDisposition::Fresh, _) => {
                self.create_fresh(working_directory, &UserSessionId::generate()?)
            }
            (ClaimDisposition::Resumed, Some(id)) => self.resume(working_directory, id, false),
            _ => Ok(candidate),
        }
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
    pub fn claim(
        &self,
        working_directory: &Path,
        proposed: &UserSessionId,
        alive: impl Fn(i32) -> bool,
    ) -> io::Result<ClaimResult> {
        let canonical = Self::canonicalize(working_directory)?;
        let directory = self.owners_directory().join(Self::fingerprint(&canonical));
        fs::create_dir_all(&directory)?;
        let existing = Self::current(&directory)?;
        let resumed = existing
            .as_ref()
            .and_then(|record| record.session_id.clone())
            .unwrap_or_else(|| proposed.value().to_owned());
        if existing
            .as_ref()
            .and_then(|record| record.process_id)
            .is_some_and(alive)
        {
            return Ok(ClaimResult {
                session_id: resumed,
                disposition: ClaimDisposition::Live,
            });
        }
        let version = existing
            .as_ref()
            .and_then(|record| record.version)
            .unwrap_or(0)
            .wrapping_add(1);
        let mut record = self.active_record(
            &intent(
                OpenOperation::CreateFresh,
                working_directory,
                proposed,
                canonical,
            ),
            version,
            i64::from(version),
            random_id()?,
        );
        record.schema_version = None;
        record.user_session_id = None;
        record.session_id = Some(resumed.clone());
        let written = write_version(&directory, &record)?;
        Ok(ClaimResult {
            session_id: if written {
                resumed
            } else {
                proposed.value().into()
            },
            disposition: if !written {
                ClaimDisposition::Contended
            } else if existing.is_none() {
                ClaimDisposition::Fresh
            } else {
                ClaimDisposition::Reclaimed
            },
        })
    }
    pub(crate) fn release(&self, lease: &SessionActivationLease) -> io::Result<()> {
        let directory = self.activation_directory(&lease.intent.session_id);
        if let Some(mut head) = Self::current(&directory)? {
            if head.generation == Some(lease.generation)
                && head.runtime_instance_id.as_deref() == Some(&lease.runtime_instance_id)
                && head.lease_id.as_deref() == Some(&lease.lease_id)
                && is_state(&head, "Active")
            {
                head.version = Some(head.version.unwrap_or(0).wrapping_add(1));
                head.state = Some("Inactive".into());
                let _ = write_version(&directory, &head)?;
            }
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
        if process.status == ProcessIdentityStatus::Found
            && (!self.runtime.process_start_tokens_comparable
                || (record
                    .process_start_token
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                    && process
                        .process_start_token
                        .as_deref()
                        .is_none_or(|s| s.trim().is_empty())))
        {
            return ActivationStatus::LiveOwnerNotDisproved;
        }
        if unknown_state {
            return ActivationStatus::Uncertain;
        }
        match process.status {
            ProcessIdentityStatus::Unreadable => ActivationStatus::Uncertain,
            ProcessIdentityStatus::Missing => ActivationStatus::Inactive,
            ProcessIdentityStatus::Found => {
                if record
                    .process_start_token
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
                {
                    ActivationStatus::Active
                } else if process
                    .process_start_token
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
                {
                    ActivationStatus::Uncertain
                } else if record.process_start_token == process.process_start_token {
                    ActivationStatus::Active
                } else {
                    ActivationStatus::Inactive
                }
            }
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
            if let Some(guard) = &self.activation_guard {
                if let Some(disposition) = guard(&intent, take_over)? {
                    return Ok(result(
                        disposition,
                        Some(intent.session_id.clone()),
                        Some(intent),
                    ));
                }
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
        let mut chunks = body.chunks_exact(4);
        let mut text: String = chunks
            .by_ref()
            .map(|chunk| {
                let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
                let code = if little_endian {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                };
                char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
            .collect();
        if !chunks.remainder().is_empty() {
            text.push(char::REPLACEMENT_CHARACTER);
        }
        return text.into();
    }
    let utf16 = bytes
        .strip_prefix(b"\xff\xfe")
        .map(|body| (body, true))
        .or_else(|| bytes.strip_prefix(b"\xfe\xff").map(|body| (body, false)));
    if let Some((body, little_endian)) = utf16 {
        let mut chunks = body.chunks_exact(2);
        let units: Vec<_> = chunks
            .by_ref()
            .map(|chunk| {
                if little_endian {
                    u16::from_le_bytes([chunk[0], chunk[1]])
                } else {
                    u16::from_be_bytes([chunk[0], chunk[1]])
                }
            })
            .collect();
        let mut text = String::from_utf16_lossy(&units);
        if !chunks.remainder().is_empty() {
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
        OwnerJsonFormatter(serde_json::ser::PrettyFormatter::with_indent(b"  ")),
    );
    record
        .serialize(&mut serializer)
        .map_err(io::Error::other)?;
    Ok(bytes)
}

struct OwnerJsonFormatter(serde_json::ser::PrettyFormatter<'static>);
macro_rules! owner_layout {
    ($method:ident $(, $argument:ident: $kind:ty)*) => {
        fn $method<W: ?Sized + io::Write>(&mut self, writer: &mut W $(, $argument: $kind)*) -> io::Result<()> {
            self.0.$method(writer $(, $argument)*)
        }
    };
}
impl serde_json::ser::Formatter for OwnerJsonFormatter {
    owner_layout!(begin_array);
    owner_layout!(end_array);
    owner_layout!(begin_array_value, first: bool);
    owner_layout!(end_array_value);
    owner_layout!(begin_object);
    owner_layout!(end_object);
    owner_layout!(begin_object_key, first: bool);
    owner_layout!(end_object_key);
    owner_layout!(begin_object_value);
    owner_layout!(end_object_value);

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for character in fragment.chars() {
            if (' '..='~').contains(&character)
                && !matches!(character, '<' | '>' | '&' | '\'' | '+' | '`')
            {
                let mut buffer = [0; 4];
                writer.write_all(character.encode_utf8(&mut buffer).as_bytes())?;
            } else {
                for unit in character.encode_utf16(&mut [0; 2]).iter() {
                    write!(writer, "\\u{unit:04X}")?;
                }
            }
        }
        Ok(())
    }
    fn write_char_escape<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        escape: serde_json::ser::CharEscape,
    ) -> io::Result<()> {
        use serde_json::ser::{CharEscape, CompactFormatter};
        match escape {
            CharEscape::Quote => writer.write_all(b"\\u0022"),
            CharEscape::AsciiControl(character) => write!(writer, "\\u{character:04X}"),
            _ => CompactFormatter.write_char_escape(writer, escape),
        }
    }
}

fn ordinal_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}
