use super::claim::WorkingDirectoryClaim;
use parrot_domain::values::identifiers::UserSessionId;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct OwnerRecord {
    pub schema_version: Option<i32>,
    pub version: Option<i32>,
    pub generation: Option<i64>,
    pub state: Option<String>,
    pub user_session_id: Option<String>,
    pub session_id: Option<String>,
    pub canonical_workspace_identity: Option<String>,
    pub working_directory: Option<String>,
    pub host_identity: Option<String>,
    pub host_key: Option<String>,
    pub boot_identity: Option<String>,
    pub process_id: Option<i32>,
    pub process_start_token: Option<String>,
    pub runtime_instance_id: Option<String>,
    pub lease_id: Option<String>,
}

impl<'de> Deserialize<'de> for OwnerRecord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OwnerVisitor;
        impl<'de> serde::de::Visitor<'de> for OwnerVisitor {
            type Value = OwnerRecord;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an owner record object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut record = OwnerRecord::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "SchemaVersion" => record.schema_version = nullable_integer(&mut map)?,
                        "Version" => record.version = nullable_integer(&mut map)?,
                        "Generation" => record.generation = nullable_integer(&mut map)?,
                        "State" => record.state = map.next_value()?,
                        "UserSessionId" => record.user_session_id = map.next_value()?,
                        "SessionId" => record.session_id = map.next_value()?,
                        "CanonicalWorkspaceIdentity" => {
                            record.canonical_workspace_identity = map.next_value()?
                        }
                        "WorkingDirectory" => record.working_directory = map.next_value()?,
                        "HostIdentity" => record.host_identity = map.next_value()?,
                        "HostKey" => record.host_key = map.next_value()?,
                        "BootIdentity" => record.boot_identity = map.next_value()?,
                        "ProcessId" => record.process_id = nullable_integer(&mut map)?,
                        "ProcessStartToken" => record.process_start_token = map.next_value()?,
                        "RuntimeInstanceId" => record.runtime_instance_id = map.next_value()?,
                        "LeaseId" => record.lease_id = map.next_value()?,
                        _ => {
                            map.next_value_seed(BoundedUnknown { parent_depth: 1 })?;
                        }
                    }
                }
                Ok(record)
            }
        }
        deserializer.deserialize_map(OwnerVisitor)
    }
}

fn nullable_integer<'de, M, T>(map: &mut M) -> Result<Option<T>, M::Error>
where
    M: serde::de::MapAccess<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = map.next_value::<Option<Box<serde_json::value::RawValue>>>()?;
    value
        .map(|value| value.get().parse().map_err(serde::de::Error::custom))
        .transpose()
}

// Raw scalar values are skipped without f64 conversion: source unknown fields
// may contain valid JSON numbers outside the native floating-point range.
struct BoundedUnknown {
    parent_depth: usize,
}
impl<'de> serde::de::DeserializeSeed<'de> for BoundedUnknown {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let container = raw.get().as_bytes().first().copied();
        if !matches!(container, Some(b'{' | b'[')) {
            return Ok(());
        }
        if self.parent_depth >= 64 {
            return Err(serde::de::Error::custom(
                "owner JSON exceeds maximum depth 64",
            ));
        }
        let mut parser = serde_json::Deserializer::from_str(raw.get());
        let visitor = UnknownContainer {
            depth: self.parent_depth + 1,
        };
        match container {
            Some(b'{') => serde::Deserializer::deserialize_map(&mut parser, visitor),
            _ => serde::Deserializer::deserialize_seq(&mut parser, visitor),
        }
        .map_err(serde::de::Error::custom)
    }
}
struct UnknownContainer {
    depth: usize,
}
impl<'de> serde::de::Visitor<'de> for UnknownContainer {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an unknown JSON container within depth 64")
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while map.next_key::<String>()?.is_some() {
            map.next_value_seed(BoundedUnknown {
                parent_depth: self.depth,
            })?;
        }
        Ok(())
    }
    fn visit_seq<S: serde::de::SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        while seq
            .next_element_seed(BoundedUnknown {
                parent_depth: self.depth,
            })?
            .is_some()
        {}
        Ok(())
    }
}

pub type ActivationGuard =
    Arc<dyn Fn(&OpenIntent, bool) -> io::Result<Option<ClaimDisposition>> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationStatus {
    Inactive,
    Active,
    Uncertain,
    /// A009 safety divergence: a local PID is live, but its persisted start
    /// timestamp does not prove a comparable process incarnation. Not overridable.
    LiveOwnerNotDisproved,
}
impl ActivationStatus {
    pub(crate) fn prevents_acquisition(self, take_over: bool) -> bool {
        matches!(self, Self::Active | Self::LiveOwnerNotDisproved)
            || (self == Self::Uncertain && !take_over)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimDisposition {
    Fresh,
    Reclaimed,
    Live,
    Contended,
    SelectionRequired,
    Corrupt,
    Resumed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessIdentityStatus {
    Missing,
    Found,
    Unreadable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub status: ProcessIdentityStatus,
    pub process_start_token: Option<String>,
}
#[derive(Clone)]
pub struct RuntimeIdentity {
    pub host_identity: String,
    pub boot_identity: String,
    pub process_id: i32,
    pub process_start_token: String,
    pub runtime_instance_id: String,
    pub inspect: Arc<dyn Fn(i32) -> ProcessIdentity + Send + Sync>,
    pub(crate) process_start_tokens_comparable: bool,
}
impl RuntimeIdentity {
    pub fn new(
        host_identity: String,
        boot_identity: String,
        process_id: i32,
        process_start_token: String,
        runtime_instance_id: String,
        inspect: Arc<dyn Fn(i32) -> ProcessIdentity + Send + Sync>,
    ) -> Self {
        Self {
            host_identity,
            boot_identity,
            process_id,
            process_start_token,
            runtime_instance_id,
            inspect,
            process_start_tokens_comparable: false,
        }
    }

    /// Assert that both persisted records and this inspector use one proven,
    /// stable incarnation scheme. Intended for controlled injected inspectors;
    /// untagged native/.NET legacy timestamps do not establish this proof.
    /// Numeric equality, proximity, or a btime conversion is not sufficient.
    pub fn with_proven_comparable_process_start_tokens(mut self) -> Self {
        self.process_start_tokens_comparable = true;
        self
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOperation {
    CreateFresh,
    Resume,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenIntent {
    pub operation: OpenOperation,
    pub session_id: UserSessionId,
    pub working_directory: String,
    pub canonical_workspace_identity: String,
}
pub struct AdmissionResult {
    pub disposition: ClaimDisposition,
    pub session_id: Option<UserSessionId>,
    pub activation_lease: Option<SessionActivationLease>,
    pub intent: Option<OpenIntent>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimResult {
    pub session_id: String,
    pub disposition: ClaimDisposition,
}
pub struct SessionActivationLease {
    claim: WorkingDirectoryClaim,
    pub intent: OpenIntent,
    pub lease_id: String,
    pub generation: i64,
    pub runtime_instance_id: String,
    released: AtomicBool,
}
impl SessionActivationLease {
    pub(crate) fn new(
        claim: WorkingDirectoryClaim,
        intent: OpenIntent,
        lease_id: String,
        generation: i64,
        runtime_instance_id: String,
    ) -> Self {
        Self {
            claim,
            intent,
            lease_id,
            generation,
            runtime_instance_id,
            released: AtomicBool::new(false),
        }
    }
    pub fn release(&self) -> io::Result<()> {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.claim.release(self)?;
        }
        Ok(())
    }
}
impl Drop for SessionActivationLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
