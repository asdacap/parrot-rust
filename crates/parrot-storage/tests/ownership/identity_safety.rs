//! A009 safety divergence. These tests do not claim source native-token parity.
use super::TempRoot;
use parrot_domain::contracts::paths::RootOrigin;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_storage::ownership::{
    ActivationStatus, ClaimDisposition, OwnerRecord, OwnershipRoots, ProcessIdentity,
    ProcessIdentityStatus, RuntimeIdentity, SessionAdmissions, WorkingDirectoryClaim,
    capture_runtime_identity, fingerprint_host, inspect_process,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn id() -> UserSessionId {
    UserSessionId::parse("protected-target").unwrap()
}
fn runtime(
    status: ProcessIdentityStatus,
    observed: Option<&str>,
    comparable: bool,
) -> RuntimeIdentity {
    let observed = observed.map(str::to_owned);
    let runtime = RuntimeIdentity::new(
        fingerprint_host("synthetic-host"),
        "boot".into(),
        4312,
        "current-start".into(),
        "reader-runtime".into(),
        Arc::new(move |_| ProcessIdentity {
            status,
            process_start_token: observed.clone(),
        }),
    );
    if comparable {
        runtime.with_proven_comparable_process_start_tokens()
    } else {
        runtime
    }
}
fn write_owner(
    root: &Path,
    workspace: &Path,
    token: Option<&str>,
    pid: i32,
) -> (std::path::PathBuf, Vec<u8>) {
    let canonical = WorkingDirectoryClaim::canonicalize(workspace).unwrap();
    let directory = root.join("owners/activations").join(id().value());
    fs::create_dir_all(&directory).unwrap();
    let record = OwnerRecord {
        schema_version: Some(2),
        version: Some(1),
        generation: Some(1),
        state: Some("Active".into()),
        user_session_id: Some(id().value().into()),
        session_id: Some(id().value().into()),
        canonical_workspace_identity: Some(canonical.clone()),
        working_directory: Some(workspace.to_string_lossy().into_owned()),
        host_identity: Some(fingerprint_host("synthetic-host")),
        process_id: Some(pid),
        process_start_token: token.map(str::to_owned),
        runtime_instance_id: Some("owner-runtime".into()),
        lease_id: Some("owner-lease".into()),
        ..OwnerRecord::default()
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    let path = directory.join("v1.json");
    fs::write(&path, &bytes).unwrap();
    let associations = root
        .join("owners/workspaces")
        .join(WorkingDirectoryClaim::fingerprint(&canonical));
    fs::create_dir_all(&associations).unwrap();
    fs::write(associations.join(format!("{}.json", id().value())), &bytes).unwrap();
    (path, bytes)
}

fn write_legacy_owner(root: &Path, workspace: &Path, token: &str, pid: i32) -> (PathBuf, Vec<u8>) {
    let canonical = WorkingDirectoryClaim::canonicalize(workspace).unwrap();
    let directory = root
        .join("owners")
        .join(WorkingDirectoryClaim::fingerprint(&canonical));
    fs::create_dir_all(&directory).unwrap();
    let record = OwnerRecord {
        version: Some(1),
        session_id: Some(id().value().into()),
        working_directory: Some(workspace.to_string_lossy().into_owned()),
        host_key: Some("synthetic-host".into()),
        process_id: Some(pid),
        process_start_token: Some(token.into()),
        ..OwnerRecord::default()
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    let path = directory.join("v1.json");
    fs::write(&path, &bytes).unwrap();
    assert!(!root.join("owners/activations").exists());
    (path, bytes)
}

// Directory entries matter too: an empty directory or a new association is a
// mutation even when no activation record or selected-session resource exists.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        if metadata.is_dir() {
            entries.insert(relative, None);
            for child in fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), entries);
            }
        } else {
            assert!(metadata.is_file(), "unexpected fixture type: {path:?}");
            entries.insert(relative, Some(fs::read(path).unwrap()));
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn reader_command(root: &Path, owner_root: &Path, owner_path: &Path, fallback: bool) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "identity_safety::native_reader_child",
            "--nocapture",
        ])
        .env("T21_A009_ROOT", root)
        .env("T21_A009_OWNER_ROOT", owner_root)
        .env("T21_A009_OWNER_PATH", owner_path)
        .env("T21_A009_FALLBACK", if fallback { "true" } else { "false" });
    command
}

fn assert_reader_succeeded(command: &mut Command, marker: &str) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout).unwrap().contains(marker));
}

#[test]
fn unproven_local_found_token_is_protected_for_exact_fractional_large_and_missing_tokens() {
    for recorded in [
        Some("639278472788359564"),
        Some("639278472788359565"),
        Some("1"),
        None,
        Some(""),
    ] {
        for observed in [Some("639278472788359564"), None, Some(" ")] {
            let temp = TempRoot::new("a009-ordinary");
            let workspace = temp.0.join("workspace");
            fs::create_dir(&workspace).unwrap();
            let root = temp.0.join("state");
            let (path, bytes) = write_owner(&root, &workspace, recorded, 4312);
            let claim = WorkingDirectoryClaim::new(
                &root,
                runtime(ProcessIdentityStatus::Found, observed, false),
            );
            let record: OwnerRecord = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                claim.determine_status(Some(&record)),
                ActivationStatus::LiveOwnerNotDisproved
            );
            assert!(claim.is_active(&id(), &workspace).unwrap());
            let before = snapshot(&temp.0);
            for takeover in [false, true] {
                let result = claim.resume(&workspace, &id(), takeover).unwrap();
                assert_eq!(result.disposition, ClaimDisposition::Live);
                assert!(result.activation_lease.is_none());
            }
            assert_eq!(snapshot(&temp.0), before);
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert!(
                !root
                    .join("owners/activations")
                    .join(id().value())
                    .join("v2.json")
                    .exists()
            );
            assert!(!root.join("sessions").exists());
        }
    }
}

#[test]
fn only_missing_or_proven_comparable_distinct_incarnation_reclaims_local_target() {
    for (status, observed, comparable, disposition) in [
        (
            ProcessIdentityStatus::Missing,
            None,
            false,
            ClaimDisposition::Resumed,
        ),
        (
            ProcessIdentityStatus::Found,
            Some("owner-start"),
            true,
            ClaimDisposition::Live,
        ),
        (
            ProcessIdentityStatus::Found,
            Some("distinct-start"),
            true,
            ClaimDisposition::Resumed,
        ),
        (
            ProcessIdentityStatus::Found,
            Some("distinct-start"),
            false,
            ClaimDisposition::Live,
        ),
        (
            ProcessIdentityStatus::Found,
            None,
            true,
            ClaimDisposition::Live,
        ),
    ] {
        let temp = TempRoot::new("a009-proof");
        let workspace = temp.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let root = temp.0.join("state");
        write_owner(&root, &workspace, Some("owner-start"), 4312);
        let claim = WorkingDirectoryClaim::new(root, runtime(status, observed, comparable));
        assert_eq!(
            claim.resume(&workspace, &id(), true).unwrap().disposition,
            disposition
        );
    }
}

#[test]
fn preactivation_recheck_protects_owner_that_becomes_observed_live_after_initial_safety_check() {
    let temp = TempRoot::new("a009-recheck");
    let workspace = temp.0.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let current = temp.0.join("parrotrust");
    let legacy = temp.0.join("parrotdotnet");
    let (path, bytes) = write_owner(
        &legacy,
        &workspace,
        Some("persisted-other-reader-token"),
        4312,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let runtime = RuntimeIdentity::new(
        fingerprint_host("synthetic-host"),
        "boot".into(),
        4312,
        "current-start".into(),
        "runtime".into(),
        Arc::new(move |_| ProcessIdentity {
            status: if count.fetch_add(1, Ordering::SeqCst) == 0 {
                ProcessIdentityStatus::Missing
            } else {
                ProcessIdentityStatus::Found
            },
            process_start_token: Some("other-reader-token".into()),
        }),
    );
    let admissions = SessionAdmissions::new(
        OwnershipRoots::new(current.clone(), legacy).unwrap(),
        runtime,
        false,
    );
    let before = snapshot(&temp.0);
    let denied = admissions.create_fresh(&workspace, &id()).unwrap();
    assert_eq!(snapshot(&temp.0), before);
    assert_eq!(denied.admission.disposition, ClaimDisposition::Live);
    assert!(denied.admission.activation_lease.is_none());
    assert!(calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(
        !current
            .join("owners/activations")
            .join(id().value())
            .join("v1.json")
            .exists()
    );
    assert!(!current.join("sessions").exists());
}

fn token_variants(owner: &RuntimeIdentity) -> [(String, &'static str); 3] {
    let native = owner.process_start_token.parse::<i64>().unwrap();
    [
        (owner.process_start_token.clone(), "equal"),
        ((native + 1).to_string(), "one-tick"),
        ("1".into(), "large"),
    ]
}

#[test]
fn actual_separate_native_readers_refuse_same_live_pid_in_either_root_with_both_fallback_modes() {
    let owner = capture_runtime_identity("synthetic-host").unwrap();
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        for legacy_only in [false, true] {
            for (token, difference) in token_variants(&owner) {
                let temp = TempRoot::new("a009-separate-readers");
                let workspace = temp.0.join("workspace");
                fs::create_dir(&workspace).unwrap();
                let root = temp.0.join(match origin {
                    RootOrigin::Current => "parrotrust",
                    RootOrigin::Legacy => "parrotdotnet",
                });
                let (path, bytes) = if legacy_only {
                    write_legacy_owner(&root, &workspace, &token, owner.process_id)
                } else {
                    write_owner(&root, &workspace, Some(&token), owner.process_id)
                };
                let before = snapshot(&temp.0);
                for fallback in [false, true] {
                    for _ in 0..2 {
                        assert_reader_succeeded(
                            reader_command(&temp.0, &root, &path, fallback)
                                .env("T21_A009_DIFFERENCE", difference),
                            "T21_A009_SEPARATE_READER_REFUSED_BOTH_TAKEOVER_SETTINGS",
                        );
                        assert_eq!(snapshot(&temp.0), before);
                        assert_eq!(fs::read(&path).unwrap(), bytes);
                        assert!(!temp.0.join("parrotrust/sessions").exists());
                        assert!(!temp.0.join("parrotdotnet/sessions").exists());
                    }
                }
            }
        }
    }
}

#[test]
fn controlled_missing_to_actual_found_preactivation_recheck_preserves_both_roots() {
    let owner = capture_runtime_identity("synthetic-host").unwrap();
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        for fallback in [false, true] {
            for (token, difference) in token_variants(&owner) {
                for action in ["fresh", "resume-false", "resume-true"] {
                    let temp = TempRoot::new("a009-native-preactivation");
                    let workspace = temp.0.join("workspace");
                    fs::create_dir(&workspace).unwrap();
                    let root = temp.0.join(match origin {
                        RootOrigin::Current => "parrotrust",
                        RootOrigin::Legacy => "parrotdotnet",
                    });
                    let (path, bytes) =
                        write_owner(&root, &workspace, Some(&token), owner.process_id);
                    // A valid selected-root association lets Resume reach its
                    // guard even when disabled fallback selects the other root.
                    let canonical = WorkingDirectoryClaim::canonicalize(&workspace).unwrap();
                    let association = temp
                        .0
                        .join("parrotrust/owners/workspaces")
                        .join(WorkingDirectoryClaim::fingerprint(&canonical));
                    fs::create_dir_all(&association).unwrap();
                    fs::write(association.join(format!("{}.json", id().value())), &bytes).unwrap();
                    if origin == RootOrigin::Legacy && fallback {
                        fs::create_dir_all(root.join("sessions").join(id().value())).unwrap();
                    }
                    let before = snapshot(&temp.0);
                    assert_reader_succeeded(
                        reader_command(&temp.0, &root, &path, fallback)
                            .env("T21_A009_DIFFERENCE", difference)
                            .env("T21_A009_MODE", action),
                        "T21_A009_CONTROLLED_MISSING_THEN_NATIVE_FOUND_REFUSED",
                    );
                    assert_eq!(snapshot(&temp.0), before);
                    assert_eq!(fs::read(path).unwrap(), bytes);
                }
            }
        }
    }
}

#[test]
fn orderly_exited_native_child_is_confirmed_missing_before_legacy_or_v2_reclaim() {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "identity_safety::native_reader_child",
            "--nocapture",
        ])
        .env("T21_A009_MODE", "orderly-exit")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let token = stdout
        .split_once("T21_A009_ORDERLY_EXIT_TOKEN ")
        .and_then(|(_, text)| text.split_whitespace().next())
        .expect("child's actual native token");
    assert_eq!(inspect_process(pid).status, ProcessIdentityStatus::Missing);
    for legacy_only in [false, true] {
        for takeover in [false, true] {
            let temp = TempRoot::new("a009-confirmed-missing");
            let workspace = temp.0.join("workspace");
            fs::create_dir(&workspace).unwrap();
            let root = temp.0.join("parrotrust");
            if legacy_only {
                write_legacy_owner(&root, &workspace, token, pid);
            } else {
                write_owner(&root, &workspace, Some(token), pid);
            }
            let claim = WorkingDirectoryClaim::new(
                root,
                capture_runtime_identity("synthetic-host").unwrap(),
            );
            let admitted = claim.resume(&workspace, &id(), takeover).unwrap();
            assert_eq!(admitted.disposition, ClaimDisposition::Resumed);
            admitted.activation_lease.unwrap().release().unwrap();
        }
    }
}

#[test]
#[ignore = "subprocess helper executed explicitly by actual-process A009 tests"]
fn native_reader_child() {
    let mode = std::env::var("T21_A009_MODE").unwrap_or_default();
    let mut runtime = capture_runtime_identity("synthetic-host").unwrap();
    if mode == "orderly-exit" {
        println!(
            "T21_A009_ORDERLY_EXIT_TOKEN {}",
            runtime.process_start_token
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("T21_A009_ROOT").expect("parent root"));
    let owner_root = PathBuf::from(std::env::var_os("T21_A009_OWNER_ROOT").unwrap());
    let owner_path = PathBuf::from(std::env::var_os("T21_A009_OWNER_PATH").unwrap());
    let record: OwnerRecord = serde_json::from_slice(&fs::read(owner_path).unwrap()).unwrap();
    let pid = record.process_id.unwrap();
    assert_ne!(pid, runtime.process_id);
    let observed = inspect_process(pid);
    assert_eq!(observed.status, ProcessIdentityStatus::Found);
    let recorded = record
        .process_start_token
        .as_ref()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let observed = observed
        .process_start_token
        .unwrap()
        .parse::<i64>()
        .unwrap();
    match std::env::var("T21_A009_DIFFERENCE").unwrap().as_str() {
        "equal" => assert_eq!(recorded, observed),
        "one-tick" => assert_eq!(recorded - observed, 1),
        "large" => assert!(recorded.abs_diff(observed) > 1),
        other => panic!("unknown token difference {other}"),
    }
    let before = snapshot(&root);
    let fallback = std::env::var("T21_A009_FALLBACK").unwrap() == "true";
    if mode.is_empty() {
        let claim = WorkingDirectoryClaim::new(owner_root, runtime.clone());
        let admissions = SessionAdmissions::new(
            OwnershipRoots::new(root.join("parrotrust"), root.join("parrotdotnet")).unwrap(),
            runtime,
            fallback,
        );
        for takeover in [false, true] {
            let direct = claim
                .resume(&root.join("workspace"), &id(), takeover)
                .unwrap();
            assert_eq!(direct.disposition, ClaimDisposition::Live);
            assert!(direct.activation_lease.is_none());
            let selected = admissions
                .resume(&root.join("workspace"), &id(), takeover)
                .unwrap();
            assert_eq!(selected.admission.disposition, ClaimDisposition::Live);
            assert!(selected.admission.activation_lease.is_none());
            assert_eq!(snapshot(&root), before);
        }
        let direct = claim.create_fresh(&root.join("workspace"), &id()).unwrap();
        assert_eq!(direct.disposition, ClaimDisposition::Live);
        assert!(direct.activation_lease.is_none());
        let selected = admissions
            .create_fresh(&root.join("workspace"), &id())
            .unwrap();
        assert_eq!(selected.admission.disposition, ClaimDisposition::Live);
        assert!(selected.admission.activation_lease.is_none());
        assert_eq!(snapshot(&root), before);
        println!("T21_A009_SEPARATE_READER_REFUSED_BOTH_TAKEOVER_SETTINGS");
    } else {
        // Only the first reported status is controlled. Both calls actually
        // inspect the live parent PID; subsequent status is the native result.
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let native_inspect = runtime.inspect.clone();
        runtime.inspect = Arc::new(move |target| {
            let actual = native_inspect(target);
            assert_eq!(target, pid);
            assert_eq!(actual.status, ProcessIdentityStatus::Found);
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                ProcessIdentity {
                    status: ProcessIdentityStatus::Missing,
                    process_start_token: None,
                }
            } else {
                actual
            }
        });
        let admissions = SessionAdmissions::new(
            OwnershipRoots::new(root.join("parrotrust"), root.join("parrotdotnet")).unwrap(),
            runtime,
            fallback,
        );
        let refused = match mode.as_str() {
            "fresh" => admissions
                .create_fresh(&root.join("workspace"), &id())
                .unwrap(),
            "resume-false" | "resume-true" => admissions
                .resume(&root.join("workspace"), &id(), mode == "resume-true")
                .unwrap(),
            other => panic!("unknown reader mode {other}"),
        };
        assert_eq!(refused.admission.disposition, ClaimDisposition::Live);
        assert!(refused.admission.activation_lease.is_none());
        assert!(calls.load(Ordering::SeqCst) >= 2);
        assert_eq!(snapshot(&root), before);
        println!("T21_A009_CONTROLLED_MISSING_THEN_NATIVE_FOUND_REFUSED");
    }
}
