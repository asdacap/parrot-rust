use super::TempRoot;
use parrot_domain::contracts::paths::RootOrigin;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_foundation::paths::{ApplicationRoots, UserSessionResources};
use parrot_storage::database::SessionDatabase;
use parrot_storage::history_files::{SessionIndex, SessionMeta};
use parrot_storage::ownership::{
    ClaimDisposition, OwnerRecord, OwnershipRoots, ProcessIdentity, ProcessIdentityStatus,
    RuntimeIdentity, SessionAdmissions, WorkingDirectoryClaim, fingerprint_host,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Fixture {
    _directory: TempRoot,
    roots: ApplicationRoots,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = TempRoot::new("selected-roots");
        let environment = HashMap::from([
            (
                "HOME".into(),
                directory.0.join("home").to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".into(),
                directory.0.join("xdg-state").to_string_lossy().into_owned(),
            ),
        ]);
        let roots = ApplicationRoots::resolve(&environment);
        assert_eq!(
            roots.current().state().parent(),
            roots.legacy().state().parent()
        );
        assert_eq!(roots.current().state().file_name().unwrap(), "parrotrust");
        assert_eq!(roots.legacy().state().file_name().unwrap(), "parrotdotnet");
        let workspace = directory.0.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            _directory: directory,
            roots,
            workspace,
        }
    }
    fn admissions(&self, status: ProcessIdentityStatus, legacy: bool) -> SessionAdmissions {
        SessionAdmissions::new(
            OwnershipRoots::new(
                self.roots.current().state().into(),
                self.roots.legacy().state().into(),
            )
            .unwrap(),
            runtime(status),
            legacy,
        )
    }
    fn claim(&self, origin: RootOrigin) -> WorkingDirectoryClaim {
        WorkingDirectoryClaim::new(self.state(origin), runtime(ProcessIdentityStatus::Found))
    }
    fn state(&self, origin: RootOrigin) -> &Path {
        match origin {
            RootOrigin::Current => self.roots.current().state(),
            RootOrigin::Legacy => self.roots.legacy().state(),
        }
    }
    fn metadata(&self, origin: RootOrigin, id: &UserSessionId, date: &str) {
        let root = self.state(origin).join("sessions").join(id.value());
        fs::create_dir_all(&root).unwrap();
        let mut meta = SessionMeta::new(
            id.value(),
            self.workspace.to_str().unwrap(),
            "provider",
            "model",
        );
        meta.created_at = Some(date.into());
        fs::write(root.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    }
    fn legacy_owner(&self, origin: RootOrigin, id: &UserSessionId) {
        let canonical = WorkingDirectoryClaim::canonicalize(&self.workspace).unwrap();
        let directory = self
            .state(origin)
            .join("owners")
            .join(WorkingDirectoryClaim::fingerprint(&canonical));
        fs::create_dir_all(&directory).unwrap();
        let record = OwnerRecord {
            version: Some(1),
            session_id: Some(id.value().into()),
            working_directory: Some(self.workspace.to_string_lossy().into_owned()),
            host_identity: Some(fingerprint_host("synthetic-host")),
            process_id: Some(4312),
            process_start_token: Some("synthetic-start".into()),
            ..OwnerRecord::default()
        };
        fs::write(
            directory.join("v1.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
}
fn runtime(status: ProcessIdentityStatus) -> RuntimeIdentity {
    RuntimeIdentity::new(
        fingerprint_host("synthetic-host"),
        "boot".into(),
        4312,
        "synthetic-start".into(),
        "runtime".into(),
        Arc::new(move |_| ProcessIdentity {
            status,
            process_start_token: Some("synthetic-start".into()),
        }),
    )
    .with_proven_comparable_process_start_tokens()
}
fn id(value: &str) -> UserSessionId {
    UserSessionId::parse(value).unwrap()
}

#[test]
fn both_roots_refuse_same_observed_live_target_even_with_takeover_and_fallback_off() {
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        let fixture = Fixture::new();
        let target = id("same-target");
        let owner = fixture
            .claim(origin)
            .create_fresh(&fixture.workspace, &target)
            .unwrap();
        let admissions = fixture.admissions(ProcessIdentityStatus::Found, false);
        for takeover in [false, true] {
            let denied = admissions
                .resume(&fixture.workspace, &target, takeover)
                .unwrap();
            assert_eq!(denied.admission.disposition, ClaimDisposition::Live);
            assert!(denied.admission.activation_lease.is_none());
        }
        let denied = admissions
            .create_fresh(&fixture.workspace, &target)
            .unwrap();
        assert_eq!(denied.admission.disposition, ClaimDisposition::Live);
        let untouched = match origin {
            RootOrigin::Current => fixture.roots.legacy().state(),
            RootOrigin::Legacy => fixture.roots.current().state(),
        };
        assert!(!untouched.exists());
        drop(owner);
    }
}

#[test]
fn distinct_live_v2_session_does_not_block_fresh_session_in_same_workspace() {
    let fixture = Fixture::new();
    let owner = fixture
        .claim(RootOrigin::Legacy)
        .create_fresh(&fixture.workspace, &id("other-session"))
        .unwrap();
    let fresh = fixture
        .admissions(ProcessIdentityStatus::Found, false)
        .create_fresh(&fixture.workspace, &id("new-session"))
        .unwrap();
    assert_eq!(fresh.admission.disposition, ClaimDisposition::Fresh);
    assert!(fresh.admission.activation_lease.is_some());
    assert!(owner.activation_lease.is_some());
}

#[test]
fn fallback_off_still_refuses_applicable_legacy_workspace_owner_without_writing_it() {
    let fixture = Fixture::new();
    fixture.legacy_owner(RootOrigin::Legacy, &id("legacy-owner"));
    let legacy_ledger = fixture.roots.legacy().state().join("owners");
    let before = directory_files(&legacy_ledger);
    let admissions = fixture.admissions(ProcessIdentityStatus::Found, false);
    assert_eq!(
        admissions
            .discover_latest(&fixture.workspace)
            .unwrap()
            .admission
            .disposition,
        ClaimDisposition::Fresh
    );
    assert_eq!(
        admissions
            .create_fresh(&fixture.workspace, &id("fresh-target"))
            .unwrap()
            .admission
            .disposition,
        ClaimDisposition::Live
    );
    assert_eq!(before, directory_files(&legacy_ledger));
    assert!(!fixture.roots.current().state().exists());
}

#[test]
fn target_v2_over_legacy_precedence_is_applied_separately_in_each_root() {
    let fixture = Fixture::new();
    let target = id("precedence-target");
    let claim = fixture.claim(RootOrigin::Legacy);
    let admitted = claim.create_fresh(&fixture.workspace, &target).unwrap();
    drop(admitted);
    fixture.legacy_owner(RootOrigin::Legacy, &id("other-legacy-owner"));
    fixture.metadata(RootOrigin::Legacy, &target, "2026-01-01");
    let resumed = fixture
        .admissions(ProcessIdentityStatus::Found, true)
        .resume(&fixture.workspace, &target, false)
        .unwrap();
    assert_eq!(resumed.admission.disposition, ClaimDisposition::Resumed);
    assert_eq!(resumed.selected_root.unwrap().origin(), RootOrigin::Legacy);
    assert!(resumed.admission.activation_lease.is_some());
}

#[test]
fn dead_target_in_other_root_does_not_block_current_fresh_target() {
    let fixture = Fixture::new();
    let target = id("dead-target");
    let owner = fixture
        .claim(RootOrigin::Legacy)
        .create_fresh(&fixture.workspace, &target)
        .unwrap();
    let current = fixture
        .admissions(ProcessIdentityStatus::Missing, false)
        .create_fresh(&fixture.workspace, &target)
        .unwrap();
    assert_eq!(current.admission.disposition, ClaimDisposition::Fresh);
    assert_eq!(current.selected_root.unwrap().origin(), RootOrigin::Current);
    drop(owner);
}

#[tokio::test]
async fn selected_legacy_resources_and_writes_remain_in_place_after_duplicate_and_flag_change() {
    let fixture = Fixture::new();
    let target = id("legacy-in-place");
    drop(
        fixture
            .claim(RootOrigin::Legacy)
            .create_fresh(&fixture.workspace, &target)
            .unwrap(),
    );
    fixture.metadata(RootOrigin::Legacy, &target, "2020-01-01");
    let admissions = fixture.admissions(ProcessIdentityStatus::Found, true);
    let admitted = admissions
        .resume(&fixture.workspace, &target, false)
        .unwrap();
    assert_eq!(admitted.admission.disposition, ClaimDisposition::Resumed);
    let selected = admitted.selected_root.unwrap();
    assert_eq!(selected.origin(), RootOrigin::Legacy);
    let before_current = fixture
        .roots
        .current()
        .state()
        .join("sessions")
        .join(target.value());
    fixture.metadata(RootOrigin::Current, &target, "2026-01-01");
    let current_before = directory_files(&before_current);
    admissions.set_legacy_path_read_fallback(false);
    let resources = UserSessionResources::new(selected.clone(), target.clone(), ()).unwrap();
    let db = SessionDatabase::open(selected.clone()).unwrap();
    let index = SessionIndex::new(selected.clone(), target.clone(), &fixture.workspace).unwrap();
    let mut meta = index.find().unwrap().unwrap();
    meta.last_opened_at = Some("opened-in-legacy".into());
    index.publish(&meta).unwrap();
    let scratch = resources.agent_scratch(&["main"]).unwrap();
    fs::write(scratch.blob_directory().join("owned.txt"), b"legacy-only").unwrap();
    let queues = parrot_storage::queue_files::QueueStore::for_session(&selected, "queues").unwrap();
    queues.create("in-place", "selected legacy queue").unwrap();
    queues
        .push(
            "in-place",
            &["legacy-item".into()],
            parrot_storage::queue_files::Direction::Back,
            false,
        )
        .unwrap();
    assert_eq!(queues.directory(), resources.queue_directory());
    assert!(
        fs::read_to_string(queues.directory().join("in-place.jsonl"))
            .unwrap()
            .contains("legacy-item")
    );
    queues.dispose().unwrap();
    let history = parrot_storage::history_files::AgentHistoryFile::new(
        selected.clone(),
        scratch.history_path(),
        vec!["agent-session-main".into()],
    )
    .unwrap();
    history
        .replace_entries(vec![
            parrot_storage::conversation::AgentHistoryEntry::Compaction {
                sequence: 1,
                summary: "legacy-history".into(),
                watermark: 0,
            },
        ])
        .await
        .unwrap();
    assert!(
        fs::read_to_string(history.path())
            .unwrap()
            .contains("legacy-history")
    );
    let blobs = parrot_storage::artifacts::output_blobs::ToolOutputBlobStore::for_session(
        &selected,
        "artifacts/output",
        Arc::new(FixedFilename),
    )
    .unwrap();
    blobs.persist("legacy-artifact", &NeverCancelled).unwrap();
    assert_eq!(
        fs::read_to_string(blobs.directory().join("selected-arse.dat")).unwrap(),
        "legacy-artifact"
    );
    assert_eq!(resources.socket_path(), selected.path().join("parrot.sock"));
    assert!(
        resources
            .socket_path()
            .starts_with(fixture.roots.legacy().state())
    );
    assert!(resources.root().starts_with(fixture.roots.legacy().state()));
    assert!(selected.path().join("session.db").is_file());
    assert_eq!(directory_files(&before_current), current_before);
    assert_eq!(
        fs::read(scratch.blob_directory().join("owned.txt")).unwrap(),
        b"legacy-only"
    );
    db.close().await.unwrap();
    let discovered = admissions.discover_latest(&fixture.workspace).unwrap();
    // With fallback disabled, the legacy-only association is not discovered.
    assert_eq!(discovered.admission.disposition, ClaimDisposition::Fresh);
}

#[test]
fn duplicate_ids_prefer_current_metadata_not_legacy_recency() {
    let fixture = Fixture::new();
    let target = id("duplicate");
    drop(
        fixture
            .claim(RootOrigin::Legacy)
            .create_fresh(&fixture.workspace, &target)
            .unwrap(),
    );
    fixture.metadata(RootOrigin::Legacy, &target, "9999");
    fixture.metadata(RootOrigin::Current, &target, "0001");
    let discovered = fixture
        .admissions(ProcessIdentityStatus::Found, true)
        .discover_latest(&fixture.workspace)
        .unwrap();
    assert_eq!(
        discovered.selected_root.unwrap().origin(),
        RootOrigin::Current
    );
    assert_eq!(discovered.admission.session_id.unwrap(), target);
}

#[test]
fn orphan_current_association_does_not_mask_newest_actual_legacy_session() {
    let fixture = Fixture::new();
    let newest = id("newest-legacy");
    let older = id("older-current");
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        drop(
            fixture
                .claim(origin)
                .create_fresh(&fixture.workspace, &newest)
                .unwrap(),
        );
    }
    fixture.metadata(RootOrigin::Legacy, &newest, "2026-10-17");
    drop(
        fixture
            .claim(RootOrigin::Current)
            .create_fresh(&fixture.workspace, &older)
            .unwrap(),
    );
    fixture.metadata(RootOrigin::Current, &older, "2020-01-01");
    let missing_current = fixture
        .roots
        .current()
        .state()
        .join("sessions")
        .join(newest.value());
    assert!(!missing_current.exists());
    let admissions = fixture.admissions(ProcessIdentityStatus::Found, true);
    let discovered = admissions.discover_latest(&fixture.workspace).unwrap();
    assert_eq!(discovered.admission.session_id.as_ref(), Some(&newest));
    assert_eq!(
        discovered.selected_root.as_ref().unwrap().origin(),
        RootOrigin::Legacy
    );
    let opened = admissions.open_default(&fixture.workspace).unwrap();
    assert_eq!(opened.admission.session_id.as_ref(), Some(&newest));
    assert_eq!(opened.admission.disposition, ClaimDisposition::Resumed);
    assert!(opened.admission.activation_lease.is_some());
    assert_eq!(opened.selected_root.unwrap().origin(), RootOrigin::Legacy);
    assert!(!missing_current.exists());
}

#[cfg(unix)]
#[test]
fn current_symlink_or_file_blocker_is_not_bypassed_via_legacy() {
    for symlink in [false, true] {
        let fixture = Fixture::new();
        let target = id("blocked");
        drop(
            fixture
                .claim(RootOrigin::Legacy)
                .create_fresh(&fixture.workspace, &target)
                .unwrap(),
        );
        fixture.metadata(RootOrigin::Legacy, &target, "2020");
        let current = fixture
            .roots
            .current()
            .state()
            .join("sessions")
            .join(target.value());
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        if symlink {
            std::os::unix::fs::symlink(
                fixture
                    .roots
                    .legacy()
                    .state()
                    .join("sessions")
                    .join(target.value()),
                &current,
            )
            .unwrap();
        } else {
            fs::write(&current, b"blocker").unwrap();
        }
        let admissions = fixture.admissions(ProcessIdentityStatus::Found, true);
        assert!(
            admissions
                .resume(&fixture.workspace, &target, false)
                .is_err()
        );
        assert!(admissions.discover_latest(&fixture.workspace).is_err());
    }
}

struct FixedFilename;
impl parrot_domain::contracts::filenames::FilenameGenerator for FixedFilename {
    fn next_filename(&self) -> String {
        "selected-arse.dat".into()
    }
}
struct NeverCancelled;
impl parrot_domain::contracts::async_ops::Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

fn directory_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                files.extend(directory_files(&entry.path()));
            } else {
                files.push((entry.path(), fs::read(entry.path()).unwrap()));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}
