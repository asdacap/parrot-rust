use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const WORKSPACE: &str = "/tmp/c/workspace/physical";

struct State(PathBuf);
impl State {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "t21-owner-records-{}",
            super::runtime::random_id().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn nullable() -> OwnerRecord {
    OwnerRecord {
        version: Some(1),
        session_id: Some("user-session-Legacy_exact.id".into()),
        working_directory: Some(WORKSPACE.into()),
        host_key: Some("host".into()),
        process_id: Some(4312),
        ..OwnerRecord::default()
    }
}
fn identity(status: ProcessIdentityStatus, observed_start: Option<String>) -> RuntimeIdentity {
    RuntimeIdentity::new(
        fingerprint_host("host"),
        "boot".into(),
        4312,
        "old-start".into(),
        "runtime".into(),
        Arc::new(move |_| ProcessIdentity {
            status,
            process_start_token: observed_start.clone(),
        }),
    )
    .with_proven_comparable_process_start_tokens()
}
fn assert_admission(
    actual: &AdmissionResult,
    disposition: ClaimDisposition,
    id: &str,
    generation: Option<i64>,
    operation: OpenOperation,
) {
    assert_eq!(actual.disposition, disposition);
    assert_eq!(actual.session_id.as_ref().unwrap().value(), id);
    assert_eq!(actual.activation_lease.is_some(), generation.is_some());
    assert_eq!(
        actual.activation_lease.as_ref().map(|lease| lease.generation),
        generation
    );
    let intent = actual.intent.as_ref().unwrap();
    assert_eq!(intent.operation, operation);
    assert_eq!(intent.session_id.value(), id);
    assert_eq!(intent.working_directory, WORKSPACE);
    assert_eq!(intent.canonical_workspace_identity, WORKSPACE);
}

#[test]
fn nullable_legacy_and_schema2_serialization_preserve_wire_format() {
    // Small literal goldens retain field order, explicit nulls and formatting.
    let legacy = r#"{
  "SchemaVersion": null,
  "Version": 1,
  "Generation": null,
  "State": null,
  "UserSessionId": null,
  "SessionId": "user-session-Legacy_exact.id",
  "CanonicalWorkspaceIdentity": null,
  "WorkingDirectory": "/tmp/c/workspace/physical",
  "HostIdentity": null,
  "HostKey": "host",
  "BootIdentity": null,
  "ProcessId": 4312,
  "ProcessStartToken": null,
  "RuntimeInstanceId": null,
  "LeaseId": null
}"#;
    assert_eq!(
        super::claim::serialize_owner(&nullable()).unwrap(),
        legacy.as_bytes()
    );
    assert_eq!(
        serde_json::from_str::<OwnerRecord>(legacy).unwrap(),
        nullable()
    );

    let active = OwnerRecord {
        schema_version: Some(2),
        version: Some(1),
        generation: Some(1),
        state: Some("Active".into()),
        user_session_id: Some("user-session-one".into()),
        session_id: Some("user-session-one".into()),
        canonical_workspace_identity: Some(WORKSPACE.into()),
        working_directory: Some(WORKSPACE.into()),
        host_identity: Some(
            "4740ae6347b0172c01254ff55bae5aff5199f4446e7f6d643d40185b3f475145".into(),
        ),
        boot_identity: Some("boot".into()),
        process_id: Some(4312),
        process_start_token: Some("old-start".into()),
        runtime_instance_id: Some("runtime".into()),
        lease_id: Some("637f16f8192c4e70b4fee2d477751e47".into()),
        ..OwnerRecord::default()
    };
    let schema2 = r#"{
  "SchemaVersion": 2,
  "Version": 1,
  "Generation": 1,
  "State": "Active",
  "UserSessionId": "user-session-one",
  "SessionId": "user-session-one",
  "CanonicalWorkspaceIdentity": "/tmp/c/workspace/physical",
  "WorkingDirectory": "/tmp/c/workspace/physical",
  "HostIdentity": "4740ae6347b0172c01254ff55bae5aff5199f4446e7f6d643d40185b3f475145",
  "HostKey": null,
  "BootIdentity": "boot",
  "ProcessId": 4312,
  "ProcessStartToken": "old-start",
  "RuntimeInstanceId": "runtime",
  "LeaseId": "637f16f8192c4e70b4fee2d477751e47"
}"#;
    assert_eq!(
        super::claim::serialize_owner(&active).unwrap(),
        schema2.as_bytes()
    );
    assert_eq!(serde_json::from_str::<OwnerRecord>(schema2).unwrap(), active);
}

#[test]
fn legacy_recovery_requires_process_evidence_even_when_boot_identity_differs() {
    use ClaimDisposition::{Live, Resumed};
    use ProcessIdentityStatus::{Found, Missing, Unreadable};

    // Boot strings alone never prove a process dead. Keep the eight historical
    // boot pairs crossed with the fourteen distinct process-evidence cases.
    for (recorded_boot, current_boot) in [
        (Some("unavailable"), "unavailable"),
        (Some("unavailable-123"), "unavailable"),
        (Some("unavailable--123"), "unavailable"),
        (Some("unavailable-123"), "unavailable-456"),
        (Some("unavailable"), "boot"),
        (Some("boot"), "unavailable"),
        (Some("other-boot"), "boot"),
        (None, "unavailable"),
    ] {
        for (recorded_start, status, observed_start, disposition, generation) in [
            (Some("old-start"), Missing, None, Resumed, Some(1)),
            (Some("old-start"), Found, Some("old-start"), Live, None),
            (Some("old-start"), Found, Some("new-start"), Resumed, Some(1)),
            (Some("old-start"), Unreadable, None, Live, None),
            (Some("old-start"), Found, None, Live, None),
            (Some("old-start"), Found, Some(""), Live, None),
            (Some("old-start"), Found, Some(" "), Live, None),
            (None, Missing, None, Resumed, Some(1)),
            (None, Found, Some("old-start"), Live, None),
            (None, Found, Some("new-start"), Live, None),
            (None, Unreadable, None, Live, None),
            (None, Found, None, Live, None),
            (None, Found, Some(""), Live, None),
            (None, Found, Some(" "), Live, None),
        ] {
            let state = State::new();
            let mut record = nullable();
            record.session_id = Some("user-session-recovery".into());
            record.boot_identity = recorded_boot.map(str::to_owned);
            record.process_start_token = recorded_start.map(str::to_owned);
            let canonical = WorkingDirectoryClaim::canonicalize(Path::new(WORKSPACE)).unwrap();
            let ledger = state
                .0
                .join("owners")
                .join(WorkingDirectoryClaim::fingerprint(&canonical));
            fs::create_dir_all(&ledger).unwrap();
            let bytes = super::claim::serialize_owner(&record).unwrap();
            fs::write(ledger.join("v1.json"), &bytes).unwrap();
            let inspected = Arc::new(AtomicUsize::new(0));
            let counter = inspected.clone();
            let runtime = RuntimeIdentity::new(
                fingerprint_host("host"),
                current_boot.into(),
                4312,
                "old-start".into(),
                "runtime".into(),
                Arc::new(move |pid| {
                    assert_eq!(pid, 4312);
                    counter.fetch_add(1, Ordering::SeqCst);
                    ProcessIdentity {
                        status,
                        process_start_token: observed_start.map(str::to_owned),
                    }
                }),
            )
            .with_proven_comparable_process_start_tokens();
            let claim = WorkingDirectoryClaim::new(&state.0, runtime);
            let actual = claim.open_default(Path::new(WORKSPACE)).unwrap();
            assert_admission(
                &actual,
                disposition,
                "user-session-recovery",
                generation,
                OpenOperation::Resume,
            );
            assert_eq!(inspected.load(Ordering::SeqCst), 2);
            assert_eq!(fs::read(ledger.join("v1.json")).unwrap(), bytes);
        }
    }
}

#[test]
fn fresh_release_is_idempotent_and_inactive_resume_advances_generation() {
    let state = State::new();
    let claim =
        WorkingDirectoryClaim::new(&state.0, identity(ProcessIdentityStatus::Missing, None));
    let id = parrot_domain::values::identifiers::UserSessionId::parse("user-session-one").unwrap();
    let fresh = claim.create_fresh(Path::new(WORKSPACE), &id).unwrap();
    assert_admission(
        &fresh,
        ClaimDisposition::Fresh,
        id.value(),
        Some(1),
        OpenOperation::CreateFresh,
    );
    let activation = state.0.join("owners/activations").join(id.value());
    let active = WorkingDirectoryClaim::current(&activation).unwrap().unwrap();
    let lease = fresh.activation_lease.as_ref().unwrap();
    let expected_active = OwnerRecord {
        schema_version: Some(2),
        version: Some(1),
        generation: Some(1),
        state: Some("Active".into()),
        user_session_id: Some(id.value().into()),
        session_id: Some(id.value().into()),
        canonical_workspace_identity: Some(WORKSPACE.into()),
        working_directory: Some(WORKSPACE.into()),
        host_identity: Some(fingerprint_host("host")),
        boot_identity: Some("boot".into()),
        process_id: Some(4312),
        process_start_token: Some("old-start".into()),
        runtime_instance_id: Some("runtime".into()),
        lease_id: Some(lease.lease_id.clone()),
        ..OwnerRecord::default()
    };
    assert_eq!(active, expected_active);
    let associations = state
        .0
        .join("owners/workspaces")
        .join(WorkingDirectoryClaim::fingerprint(WORKSPACE))
        .join(format!("{}.json", id.value()));
    let association: OwnerRecord = serde_json::from_slice(&fs::read(associations).unwrap()).unwrap();
    assert_eq!(
        association,
        OwnerRecord {
            schema_version: Some(2),
            user_session_id: Some(id.value().into()),
            session_id: Some(id.value().into()),
            canonical_workspace_identity: Some(WORKSPACE.into()),
            working_directory: Some(WORKSPACE.into()),
            ..OwnerRecord::default()
        }
    );
    lease.release().unwrap();
    let inactive_bytes = fs::read(activation.join("v2.json")).unwrap();
    lease.release().unwrap();
    assert_eq!(fs::read(activation.join("v2.json")).unwrap(), inactive_bytes);
    assert!(!activation.join("v3.json").exists());
    let mut inactive = expected_active;
    inactive.version = Some(2);
    inactive.state = Some("Inactive".into());
    assert_eq!(
        WorkingDirectoryClaim::current(&activation).unwrap(),
        Some(inactive)
    );
    let resumed = claim.open_default(Path::new(WORKSPACE)).unwrap();
    assert_admission(
        &resumed,
        ClaimDisposition::Resumed,
        id.value(),
        Some(2),
        OpenOperation::Resume,
    );
    let head = WorkingDirectoryClaim::current(&activation).unwrap().unwrap();
    assert_eq!(head.version, Some(3));
    assert_eq!(head.generation, Some(2));
    assert_eq!(head.state.as_deref(), Some("Active"));
}

#[test]
fn active_owner_blocks_default_open_and_explicit_takeover() {
    let state = State::new();
    let claim = WorkingDirectoryClaim::new(
        &state.0,
        identity(ProcessIdentityStatus::Found, Some("old-start".into())),
    );
    let id =
        parrot_domain::values::identifiers::UserSessionId::parse("user-session-active").unwrap();
    let owner = claim.create_fresh(Path::new(WORKSPACE), &id).unwrap();
    for refused in [
        claim.open_default(Path::new(WORKSPACE)).unwrap(),
        claim.resume(Path::new(WORKSPACE), &id, true).unwrap(),
    ] {
        assert_admission(
            &refused,
            ClaimDisposition::Live,
            id.value(),
            None,
            OpenOperation::Resume,
        );
    }
    let activation = state.0.join("owners/activations").join(id.value());
    assert_eq!(
        WorkingDirectoryClaim::current(&activation).unwrap().unwrap().version,
        Some(1)
    );
    assert!(!activation.join("v2.json").exists());
    drop(owner);
}

#[test]
fn pid_reuse_recovers_ownership_and_stale_release_cannot_fence_the_successor() {
    let state = State::new();
    let old = WorkingDirectoryClaim::new(
        &state.0,
        identity(ProcessIdentityStatus::Found, Some("old-start".into())),
    );
    let id =
        parrot_domain::values::identifiers::UserSessionId::parse("user-session-reuse").unwrap();
    let owner = old.create_fresh(Path::new(WORKSPACE), &id).unwrap();
    let reused = WorkingDirectoryClaim::new(
        &state.0,
        identity(ProcessIdentityStatus::Found, Some("new-start".into())),
    );
    let successor = reused.open_default(Path::new(WORKSPACE)).unwrap();
    assert_admission(
        &successor,
        ClaimDisposition::Resumed,
        id.value(),
        Some(2),
        OpenOperation::Resume,
    );
    let activation = state.0.join("owners/activations").join(id.value());
    let before = fs::read(activation.join("v2.json")).unwrap();
    owner.activation_lease.as_ref().unwrap().release().unwrap();
    assert_eq!(fs::read(activation.join("v2.json")).unwrap(), before);
    assert!(!activation.join("v3.json").exists());
    let head = WorkingDirectoryClaim::current(&activation).unwrap().unwrap();
    assert_eq!(head.generation, Some(2));
    assert_eq!(head.state.as_deref(), Some("Active"));
    assert_eq!(
        head.lease_id.as_deref(),
        Some(successor.activation_lease.as_ref().unwrap().lease_id.as_str())
    );
    successor.activation_lease.as_ref().unwrap().release().unwrap();
    let head = WorkingDirectoryClaim::current(&activation).unwrap().unwrap();
    assert_eq!(head.version, Some(3));
    assert_eq!(head.generation, Some(2));
    assert_eq!(head.state.as_deref(), Some("Inactive"));
}
