use super::claim::WorkingDirectoryClaim;
use super::runtime::{fingerprint_host, random_id};
use super::types::*;
use parrot_domain::values::identifiers::UserSessionId;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("parrot-claim-{}", random_id().unwrap()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self { root, workspace }
    }
    fn claim(
        &self,
        host: &str,
        status: ProcessIdentityStatus,
        start: Option<&str>,
    ) -> WorkingDirectoryClaim {
        let start = start.map(str::to_owned);
        WorkingDirectoryClaim::new(
            &self.root,
            RuntimeIdentity::new(
                fingerprint_host(host),
                "boot".into(),
                4312,
                "start".into(),
                random_id().unwrap(),
                Arc::new(move |_| ProcessIdentity {
                    status,
                    process_start_token: start.clone(),
                }),
            )
            .with_proven_comparable_process_start_tokens(),
        )
    }
    fn canonical(&self) -> String {
        WorkingDirectoryClaim::canonicalize(&self.workspace).unwrap()
    }
    fn activation(&self, id: &UserSessionId) -> PathBuf {
        self.root.join("owners/activations").join(id.value())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn id(value: &str) -> UserSessionId {
    UserSessionId::parse(value).unwrap()
}
fn write(path: &Path, record: &OwnerRecord) {
    fs::write(path, serde_json::to_vec(record).unwrap()).unwrap();
}

#[test]
fn owner_record_duplicates_are_last_wins_but_validate_every_known_value() {
    let parsed: OwnerRecord = serde_json::from_str(r#"{"Version":1,"Unknown":false,"Version":2,"SessionId":"old","SessionId":null,"Unknown":{"nested":[1]}}"#).unwrap();
    assert_eq!(parsed.version, Some(2));
    assert_eq!(parsed.session_id, None);
    for key in [
        "SchemaVersion",
        "Version",
        "Generation",
        "State",
        "UserSessionId",
        "SessionId",
        "CanonicalWorkspaceIdentity",
        "WorkingDirectory",
        "HostIdentity",
        "HostKey",
        "BootIdentity",
        "ProcessId",
        "ProcessStartToken",
        "RuntimeInstanceId",
        "LeaseId",
    ] {
        let value = match key {
            "SchemaVersion" | "Version" | "Generation" | "ProcessId" => "1",
            _ => "\"value\"",
        };
        let json = format!("{{\"{key}\":{value},\"{key}\":null}}");
        let record: OwnerRecord = serde_json::from_str(&json).unwrap();
        assert!(serde_json::to_value(record).unwrap()[key].is_null());
        let invalid = format!("{{\"{key}\":true,\"{key}\":{value}}}");
        assert!(
            serde_json::from_str::<OwnerRecord>(&invalid).is_err(),
            "{key}"
        );
    }
    for invalid in [
        r#"{"Version":"bad","Version":1}"#,
        r#"{"Version":2147483648,"Version":1}"#,
        r#"{"ProcessId":-2147483649,"ProcessId":1}"#,
        r#"{"Generation":9223372036854775808,"Generation":1}"#,
        r#"{"Version":1.5,"Version":1}"#,
    ] {
        assert!(
            serde_json::from_str::<OwnerRecord>(invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn owner_serialization_preserves_source_order_indent_nulls_and_escapes() {
    let record = OwnerRecord {
        state: Some("<> &'+`\"\\\n\t\r\u{8}\u{c}\u{1f}é雪😀\u{7f}".into()),
        ..Default::default()
    };
    let actual = super::claim::serialize_owner(&record).unwrap();
    let expected = concat!(
        "{\n  \"SchemaVersion\": null,\n  \"Version\": null,\n  \"Generation\": null,\n",
        "  \"State\": \"\\u003C\\u003E \\u0026\\u0027\\u002B\\u0060\\u0022\\\\\\n\\t\\r\\b\\f\\u001F\\u00E9\\u96EA\\uD83D\\uDE00\\u007F\",\n",
        "  \"UserSessionId\": null,\n  \"SessionId\": null,\n  \"CanonicalWorkspaceIdentity\": null,\n",
        "  \"WorkingDirectory\": null,\n  \"HostIdentity\": null,\n  \"HostKey\": null,\n",
        "  \"BootIdentity\": null,\n  \"ProcessId\": null,\n  \"ProcessStartToken\": null,\n",
        "  \"RuntimeInstanceId\": null,\n  \"LeaseId\": null\n}"
    );
    assert_eq!(actual, expected.as_bytes());
    assert_eq!(
        serde_json::from_slice::<OwnerRecord>(&actual).unwrap(),
        record
    );
}

#[test]
fn activation_guard_runs_on_every_collision_retry_and_preserves_unleased_intent() {
    use std::sync::atomic::AtomicUsize;
    let f = Fixture::new();
    let session = id("user-session-guarded");
    let directory = f.activation(&session);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("v1.json"), "bad").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let claim = f
        .claim("host", ProcessIdentityStatus::Missing, None)
        .with_activation_guard(Arc::new(move |intent, takeover| {
            assert_eq!(intent.session_id.value(), "user-session-guarded");
            assert!(!takeover);
            Ok((observed.fetch_add(1, Ordering::SeqCst) == 1).then_some(ClaimDisposition::Live))
        }));
    let result = claim.create_fresh(&f.workspace, &session).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.disposition, ClaimDisposition::Live);
    assert_eq!(result.session_id, Some(session));
    assert_eq!(result.intent.unwrap().operation, OpenOperation::CreateFresh);
    assert!(result.activation_lease.is_none());
    assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn admission_safety_uses_any_legacy_id_unless_target_v2_supersedes_it() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Found, Some("start"));
    let target = id("user-session-target");
    let canonical = f.canonical();
    claim
        .claim(&f.workspace, &id("user-session-legacy"), |_| false)
        .unwrap();
    assert_eq!(
        claim.activation_status(&target, &canonical).unwrap(),
        (ActivationStatus::Inactive, false)
    );
    assert_eq!(
        claim.admission_safety_status(&target, &canonical).unwrap(),
        (ActivationStatus::Active, false)
    );
    let distinct = claim
        .create_fresh(&f.workspace, &id("user-session-distinct"))
        .unwrap();
    assert_eq!(
        claim.admission_safety_status(&target, &canonical).unwrap(),
        (ActivationStatus::Active, false)
    );
    let created = claim.create_fresh(&f.workspace, &target).unwrap();
    created.activation_lease.unwrap().release().unwrap();
    assert_eq!(
        claim.admission_safety_status(&target, &canonical).unwrap(),
        (ActivationStatus::Inactive, false)
    );
    assert_eq!(
        claim
            .workspace_associations(&canonical)
            .unwrap()
            .unwrap()
            .len(),
        3
    );
    let mut head = WorkingDirectoryClaim::current(&f.activation(&target))
        .unwrap()
        .unwrap();
    head.version = Some(head.version.unwrap() + 1);
    head.session_id = Some("wrong".into());
    head.user_session_id = Some("wrong".into());
    write(&f.activation(&target).join("v99.json"), &head);
    assert_eq!(
        claim.admission_safety_status(&target, &canonical).unwrap(),
        (ActivationStatus::Uncertain, true)
    );
    drop(distinct);
}

#[test]
fn owner_record_preserves_nullable_pascal_case_shape_and_legacy_fields() {
    let record: OwnerRecord = serde_json::from_str(
        r#"{"Version":1,"SessionId":"user-session-Legacy_exact.id","Unknown":true}"#,
    )
    .unwrap();
    assert_eq!(
        record.session_id.as_deref(),
        Some("user-session-Legacy_exact.id")
    );
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 15);
    assert!(json["UserSessionId"].is_null());
    assert_eq!(json["Version"], 1);
    assert!(serde_json::from_str::<OwnerRecord>(r#"{"Version":"1"}"#).is_err());
}

#[test]
fn current_uses_positive_record_version_not_filename_and_ignores_bad_files() {
    let f = Fixture::new();
    let directory = f.root.join("ledger");
    fs::create_dir_all(directory.join("v100.json")).unwrap();
    for (name, version) in [
        ("v1000.json", -1),
        ("v1.json", 9),
        ("v99.json", 2),
        ("ignored.json", 100),
    ] {
        write(
            &directory.join(name),
            &OwnerRecord {
                version: Some(version),
                ..Default::default()
            },
        );
    }
    fs::write(directory.join("v999.json"), "broken").unwrap();
    fs::write(directory.join("v998.json"), r#"{"Version":2147483648}"#).unwrap();
    assert_eq!(
        WorkingDirectoryClaim::current(&directory)
            .unwrap()
            .unwrap()
            .version,
        Some(9)
    );
    assert!(
        WorkingDirectoryClaim::current(&directory.join("missing"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn default_open_resumes_and_ordinally_selects_inactive_associations() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
    for value in ["user-session-two", "user-session-one"] {
        let opened = claim.create_fresh(&f.workspace, &id(value)).unwrap();
        opened.activation_lease.unwrap().release().unwrap();
    }
    let discovery = claim.discover_latest(&f.workspace).unwrap();
    assert_eq!(discovery.session_id.unwrap().value(), "user-session-one");
    assert!(discovery.activation_lease.is_none());
    let resumed = claim.open_default(&f.workspace).unwrap();
    assert_eq!(resumed.disposition, ClaimDisposition::Resumed);
    assert_eq!(resumed.session_id.unwrap().value(), "user-session-one");
}

#[test]
fn live_candidate_is_never_taken_over() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Found, Some("start"));
    let session = id("user-session-one");
    let first = claim.create_fresh(&f.workspace, &session).unwrap();
    assert_eq!(first.disposition, ClaimDisposition::Fresh);
    let second = claim.open_default(&f.workspace).unwrap();
    assert_eq!(second.disposition, ClaimDisposition::Live);
    assert_eq!(second.session_id.as_ref(), Some(&session));
    assert!(second.activation_lease.is_none());
    let takeover = claim.resume(&f.workspace, &session, true).unwrap();
    assert_eq!(takeover.disposition, ClaimDisposition::Live);
    assert!(takeover.activation_lease.is_none());
    assert!(claim.is_active(&session, &f.workspace).unwrap());
    drop(first);
    assert!(!claim.is_active(&session, &f.workspace).unwrap());
}

#[test]
fn foreign_host_does_not_inspect_pid_and_requires_explicit_takeover() {
    let f = Fixture::new();
    let foreign = f.claim("foreign", ProcessIdentityStatus::Found, Some("start"));
    let session = id("user-session-foreign");
    let first = foreign.create_fresh(&f.workspace, &session).unwrap();
    let inspected = Arc::new(AtomicBool::new(false));
    let marker = inspected.clone();
    let local = WorkingDirectoryClaim::new(
        &f.root,
        RuntimeIdentity::new(
            fingerprint_host("local"),
            "boot".into(),
            4312,
            "start".into(),
            "local".into(),
            Arc::new(move |_| {
                marker.store(true, Ordering::SeqCst);
                ProcessIdentity {
                    status: ProcessIdentityStatus::Missing,
                    process_start_token: None,
                }
            }),
        )
        .with_proven_comparable_process_start_tokens(),
    );
    let result = local.open_default(&f.workspace).unwrap();
    assert_eq!(result.disposition, ClaimDisposition::Live);
    assert_eq!(result.session_id.as_ref(), Some(&session));
    let takeover = local.resume(&f.workspace, &session, true).unwrap();
    assert!(!inspected.load(Ordering::SeqCst));
    assert_eq!(takeover.disposition, ClaimDisposition::Resumed);
    assert!(takeover.activation_lease.is_some());
    drop(first);
    assert_eq!(
        WorkingDirectoryClaim::current(&f.activation(&session))
            .unwrap()
            .unwrap()
            .runtime_instance_id
            .as_deref(),
        Some("local")
    );
}

#[test]
fn reused_pid_with_exact_original_session_id_is_inactive() {
    let f = Fixture::new();
    let first = f.claim("host", ProcessIdentityStatus::Found, Some("old-start"));
    let session = id("user-session-reused-pid");
    let owner = first.create_fresh(&f.workspace, &session).unwrap();
    let second = f.claim("host", ProcessIdentityStatus::Found, Some("new-start"));
    let result = second.open_default(&f.workspace).unwrap();
    assert_eq!(result.disposition, ClaimDisposition::Resumed);
    assert_eq!(result.session_id.as_ref(), Some(&session));
    drop(owner);
}

#[test]
fn boot_identity_does_not_override_process_liveness_matrix() {
    let f = Fixture::new();
    for boot in [
        None,
        Some("unavailable"),
        Some("unavailable-123"),
        Some("unavailable--123"),
        Some("other-boot"),
        Some("boot"),
    ] {
        for recorded_start in [None, Some("old-start"), Some(" ")] {
            for (status, start, expected) in [
                (
                    ProcessIdentityStatus::Missing,
                    None,
                    ActivationStatus::Inactive,
                ),
                (
                    ProcessIdentityStatus::Unreadable,
                    None,
                    ActivationStatus::Uncertain,
                ),
                (
                    ProcessIdentityStatus::Found,
                    Some("old-start"),
                    ActivationStatus::Active,
                ),
                (
                    ProcessIdentityStatus::Found,
                    Some("new-start"),
                    ActivationStatus::Inactive,
                ),
                (
                    ProcessIdentityStatus::Found,
                    None,
                    ActivationStatus::LiveOwnerNotDisproved,
                ),
                (
                    ProcessIdentityStatus::Found,
                    Some(""),
                    ActivationStatus::LiveOwnerNotDisproved,
                ),
                (
                    ProcessIdentityStatus::Found,
                    Some(" "),
                    ActivationStatus::LiveOwnerNotDisproved,
                ),
            ] {
                let claim = f.claim("host", status, start);
                let record = OwnerRecord {
                    host_key: Some("host".into()),
                    process_id: Some(4312),
                    boot_identity: boot.map(str::to_owned),
                    process_start_token: recorded_start.map(str::to_owned),
                    ..Default::default()
                };
                let expected = if status == ProcessIdentityStatus::Found
                    && recorded_start.is_none_or(|s| s.trim().is_empty())
                {
                    ActivationStatus::Active
                } else {
                    expected
                };
                assert_eq!(
                    claim.determine_status(Some(&record)),
                    expected,
                    "{boot:?} {recorded_start:?} {status:?} {start:?}"
                );
            }
        }
    }
}

#[test]
fn legacy_owner_preserves_exact_id_and_nullable_fields() {
    let f = Fixture::new();
    let directory = f
        .root
        .join("owners")
        .join(WorkingDirectoryClaim::fingerprint(&f.canonical()));
    fs::create_dir_all(&directory).unwrap();
    write(
        &directory.join("v1.json"),
        &OwnerRecord {
            version: Some(1),
            session_id: Some("user-session-Legacy_exact.id".into()),
            working_directory: Some(f.workspace.to_string_lossy().into_owned()),
            host_key: Some("host".into()),
            process_id: Some(4312),
            ..Default::default()
        },
    );
    let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
    let opened = claim.open_default(&f.workspace).unwrap();
    assert_eq!(opened.disposition, ClaimDisposition::Resumed);
    assert_eq!(
        opened.session_id.unwrap().value(),
        "user-session-Legacy_exact.id"
    );
}

#[test]
fn release_is_idempotent_and_fenced_individually_by_each_identity() {
    for field in [
        "source-all",
        "generation",
        "runtime",
        "lease",
        "state",
        "none",
    ] {
        let f = Fixture::new();
        let claim = f.claim("host", ProcessIdentityStatus::Found, Some("start"));
        let session = id("user-session-fenced");
        let opened = claim.create_fresh(&f.workspace, &session).unwrap();
        let lease = opened.activation_lease.unwrap();
        let directory = f.activation(&session);
        let mut replacement = WorkingDirectoryClaim::current(&directory).unwrap().unwrap();
        replacement.version = Some(2);
        match field {
            "source-all" => {
                replacement.generation = Some(2);
                replacement.runtime_instance_id = Some("replacement-runtime".into());
                replacement.lease_id = Some("replacement-lease".into());
            }
            "generation" => replacement.generation = Some(99),
            "runtime" => replacement.runtime_instance_id = Some("replacement".into()),
            "lease" => replacement.lease_id = Some("replacement".into()),
            "state" => replacement.state = Some("Inactive".into()),
            _ => {}
        }
        write(&directory.join("v2.json"), &replacement);
        lease.release().unwrap();
        lease.release().unwrap();
        assert_eq!(directory.join("v3.json").exists(), field == "none");
        assert!(!directory.join("v4.json").exists());
        assert!(directory.join("v1.json").exists());
        assert!(directory.join("v2.json").exists());
        if field != "none" {
            assert_eq!(
                WorkingDirectoryClaim::current(&directory).unwrap(),
                Some(replacement)
            );
        }
    }
}

#[test]
fn concurrent_resume_grants_exactly_one_activation_and_cleans_staging() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Found, Some("start"));
    let session = id("user-session-contended");
    claim
        .create_fresh(&f.workspace, &session)
        .unwrap()
        .activation_lease
        .unwrap()
        .release()
        .unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let attempts: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let claim = &claim;
                let workspace = &f.workspace;
                let session = &session;
                scope.spawn(move || {
                    barrier.wait();
                    claim.resume(workspace, session, false).unwrap()
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect()
    });
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.activation_lease.is_some())
            .count(),
        1
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.disposition == ClaimDisposition::Live)
            .count(),
        7
    );
    assert!(fs::read_dir(f.activation(&session)).unwrap().all(|file| {
        !file
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")
    }));
}

#[test]
fn corrupt_association_activation_and_metadata_fail_closed() {
    for location in ["association", "activation", "metadata"] {
        let f = Fixture::new();
        let claim = f.claim("host", ProcessIdentityStatus::Unreadable, None);
        let session = id("user-session-corrupt");
        let first = claim.create_fresh(&f.workspace, &session).unwrap();
        match location {
            "association" => fs::write(
                f.root
                    .join("owners/workspaces")
                    .join(WorkingDirectoryClaim::fingerprint(&f.canonical()))
                    .join("unexpected.json"),
                "bad",
            )
            .unwrap(),
            "activation" => {
                let mut record = WorkingDirectoryClaim::current(&f.activation(&session))
                    .unwrap()
                    .unwrap();
                record.version = Some(2);
                record.canonical_workspace_identity = Some("wrong".into());
                write(&f.activation(&session).join("v2.json"), &record);
            }
            _ => {
                let directory = f.root.join("sessions").join(session.value());
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join("meta.json"), "bad").unwrap();
            }
        }
        let discovered = claim.open_default(&f.workspace).unwrap();
        assert_eq!(discovered.disposition, ClaimDisposition::Corrupt);
        assert!(discovered.activation_lease.is_none());
        drop(first);
    }
}

#[test]
fn discovery_uses_successful_open_recency_without_activation() {
    for (count, reopened, expected) in [
        (0, false, ""),
        (1, false, "user-session-one"),
        (2, false, "user-session-two"),
        (2, true, "user-session-one"),
    ] {
        let f = Fixture::new();
        let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
        for index in 0..count {
            let (name, created) = if index == 0 {
                ("user-session-one", "2026-01-01T00:00:00Z")
            } else {
                ("user-session-two", "2026-02-01T00:00:00Z")
            };
            let opened = if reopened && index == 0 {
                "2026-03-01T00:00:00Z"
            } else {
                ""
            };
            let session = id(name);
            drop(claim.create_fresh(&f.workspace, &session).unwrap());
            let directory = f.root.join("sessions").join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("meta.json"), serde_json::to_vec(&serde_json::json!({"Id":name,"WorkingDirectory":f.workspace,"ProviderId":"unused","Model":"model","CreatedAt":created,"LastOpenedAt":opened})).unwrap()).unwrap();
        }
        let other_workspace = f.root.join("other-workspace");
        fs::create_dir_all(&other_workspace).unwrap();
        drop(
            claim
                .create_fresh(&other_workspace, &id("user-session-other"))
                .unwrap(),
        );
        let discovered = claim.discover_latest(&f.workspace).unwrap();
        assert_eq!(
            discovered.disposition,
            if count == 0 {
                ClaimDisposition::Fresh
            } else {
                ClaimDisposition::Resumed
            }
        );
        assert_eq!(
            discovered.session_id.as_ref().map_or("", |id| id.value()),
            expected
        );
        assert!(discovered.activation_lease.is_none());
    }
}

#[test]
fn unreadable_owner_with_and_without_corrupt_metadata_is_not_activated() {
    for corrupt in [false, true] {
        let f = Fixture::new();
        let claim = f.claim("host", ProcessIdentityStatus::Unreadable, None);
        let session = id("user-session-uncertain");
        let first = claim.create_fresh(&f.workspace, &session).unwrap();
        if corrupt {
            let directory = f.root.join("sessions").join(session.value());
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("meta.json"), "not-json").unwrap();
        }
        let result = claim.open_default(&f.workspace).unwrap();
        assert_eq!(
            result.disposition,
            if corrupt {
                ClaimDisposition::Corrupt
            } else {
                ClaimDisposition::Live
            }
        );
        assert!(result.activation_lease.is_none());
        drop(first);
    }
}

#[test]
fn invalid_session_id_is_rejected_before_admission_filesystem_access() {
    let f = Fixture::new();
    let untouched = f.root.join("untouched-state");
    let claim = WorkingDirectoryClaim::new(
        &untouched,
        RuntimeIdentity::new(
            fingerprint_host("host"),
            "boot".into(),
            4312,
            "start".into(),
            "runtime".into(),
            Arc::new(|_| ProcessIdentity {
                status: ProcessIdentityStatus::Missing,
                process_start_token: None,
            }),
        ),
    );
    let parsed = UserSessionId::parse("../user-session-other");
    assert!(parsed.is_err());
    let attempt = parsed.map(|session| claim.resume(&f.workspace, &session, false));
    assert!(attempt.is_err());
    assert!(!untouched.exists());
}

#[test]
fn canonicalize_allows_missing_directories_and_normalizes_lexically() {
    let f = Fixture::new();
    let missing = f.workspace.join("missing/../future/");
    assert_eq!(
        WorkingDirectoryClaim::canonicalize(&missing).unwrap(),
        f.workspace.join("future").to_string_lossy()
    );
    assert!(WorkingDirectoryClaim::canonicalize(Path::new(" ")).is_err());
}

#[cfg(unix)]
#[test]
fn canonical_workspace_identity_matches_final_symbolic_link_only() {
    let f = Fixture::new();
    let alias = f.root.join("workspace-alias");
    std::os::unix::fs::symlink(&f.workspace, &alias).unwrap();
    let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
    drop(
        claim
            .create_fresh(&alias, &id("user-session-linked"))
            .unwrap(),
    );
    let resumed = claim.open_default(&f.workspace).unwrap();
    assert_eq!(resumed.disposition, ClaimDisposition::Resumed);
    assert_eq!(resumed.session_id.unwrap().value(), "user-session-linked");
    assert_eq!(
        resumed.intent.unwrap().working_directory,
        f.workspace.to_string_lossy()
    );
    fs::create_dir(f.workspace.join("child")).unwrap();
    assert_eq!(
        WorkingDirectoryClaim::canonicalize(&alias.join("child")).unwrap(),
        alias.join("child").to_string_lossy()
    );
}

#[test]
fn blocked_version_exhausts_activation_retries_without_leaving_staging() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
    let session = id("user-session-blocked");
    let directory = f.activation(&session);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("v1.json"), "corrupt").unwrap();
    let result = claim.create_fresh(&f.workspace, &session).unwrap();
    assert_eq!(result.disposition, ClaimDisposition::Contended);
    assert!(result.activation_lease.is_none());
    assert_eq!(
        fs::read_to_string(directory.join("v1.json")).unwrap(),
        "corrupt"
    );
    assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn legacy_claim_returns_live_then_reclaims_exact_session() {
    let f = Fixture::new();
    let claim = f.claim("host", ProcessIdentityStatus::Missing, None);
    let first = claim
        .claim(&f.workspace, &id("user-session-Original.id"), |_| false)
        .unwrap();
    assert_eq!(first.disposition, ClaimDisposition::Fresh);
    let live = claim
        .claim(&f.workspace, &id("user-session-other"), |_| true)
        .unwrap();
    assert_eq!(live.disposition, ClaimDisposition::Live);
    assert_eq!(live.session_id, first.session_id);
    let reclaimed = claim
        .claim(&f.workspace, &id("user-session-other"), |_| false)
        .unwrap();
    assert_eq!(reclaimed.disposition, ClaimDisposition::Reclaimed);
    assert_eq!(reclaimed.session_id, first.session_id);
}

#[test]
fn default_inactive_candidate_is_rechecked_live_before_any_activation_write() {
    let f = Fixture::new();
    let session = id("user-session-recheck");
    f.claim("host", ProcessIdentityStatus::Missing, None)
        .claim(&f.workspace, &session, |_| false)
        .unwrap();
    let ledger = f
        .root
        .join("owners")
        .join(WorkingDirectoryClaim::fingerprint(&f.canonical()))
        .join("v1.json");
    let before = fs::read(&ledger).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inspected = calls.clone();
    let runtime = RuntimeIdentity::new(
        fingerprint_host("host"),
        "boot".into(),
        4312,
        "reader-start".into(),
        "reader".into(),
        Arc::new(move |_| ProcessIdentity {
            status: if inspected.fetch_add(1, Ordering::SeqCst) == 0 {
                ProcessIdentityStatus::Missing
            } else {
                ProcessIdentityStatus::Found
            },
            process_start_token: Some("different-reader-start".into()),
        }),
    );
    let refused = WorkingDirectoryClaim::new(&f.root, runtime)
        .open_default(&f.workspace)
        .unwrap();
    assert_eq!(refused.disposition, ClaimDisposition::Live);
    assert!(refused.activation_lease.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(fs::read(ledger).unwrap(), before);
    assert!(!f.root.join("owners/activations").exists());
    assert!(!f.root.join("owners/workspaces").exists());
    assert!(!f.root.join("sessions").exists());
}

#[test]
fn fresh_refusing_guard_does_not_create_owner_directories_or_associations() {
    let f = Fixture::new();
    let claim = f
        .claim("host", ProcessIdentityStatus::Missing, None)
        .with_activation_guard(Arc::new(|_, _| Ok(Some(ClaimDisposition::Live))));
    let result = claim
        .create_fresh(&f.workspace, &id("user-session-refused"))
        .unwrap();
    assert_eq!(result.disposition, ClaimDisposition::Live);
    assert!(result.activation_lease.is_none());
    assert!(!f.root.join("owners").exists());
    assert!(!f.root.join("sessions").exists());
}

#[test]
fn fresh_association_publication_failure_releases_only_its_acquired_activation() {
    let f = Fixture::new();
    let session = id("user-session-association-failure");
    let association = f
        .root
        .join("owners/workspaces")
        .join(WorkingDirectoryClaim::fingerprint(&f.canonical()))
        .join(format!("{}.json", session.value()));
    let raced = association.clone();
    let claim = f
        .claim("host", ProcessIdentityStatus::Missing, None)
        .with_activation_guard(Arc::new(move |_, _| {
            fs::create_dir_all(raced.parent().unwrap())?;
            fs::write(&raced, b"corrupt-successor-association")?;
            Ok(None)
        }));
    let result = claim.create_fresh(&f.workspace, &session).unwrap();
    assert_eq!(result.disposition, ClaimDisposition::Corrupt);
    assert!(result.activation_lease.is_none());
    assert_eq!(
        fs::read(association).unwrap(),
        b"corrupt-successor-association"
    );
    let head = WorkingDirectoryClaim::current(&f.activation(&session))
        .unwrap()
        .unwrap();
    assert_eq!(head.state.as_deref(), Some("Inactive"));
    assert_eq!(head.generation, Some(1));
    assert!(!f.root.join("sessions").exists());
}
