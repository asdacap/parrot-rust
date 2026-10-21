use super::support::*;
use parrot_runtime::plan_mode::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, sync::Arc};

#[test]
fn prepare_owns_private_correlated_random_files_and_reentry_preserves_bytes() {
    let fixture = Fixture::new();
    let store = PlanArtifactStore::new(fixture.scratch("main"));
    assert!(store.paths().is_none());
    store.prepare().unwrap();
    let paths = store.paths().unwrap();
    assert_eq!(paths.plan().parent().unwrap(), store.plan_directory());
    let name = paths.plan().file_name().unwrap().to_str().unwrap();
    let token = name
        .strip_prefix("plan-")
        .unwrap()
        .strip_suffix(".md")
        .unwrap();
    assert_eq!(token.len(), 32);
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(
        paths.tasks(),
        store
            .plan_directory()
            .join(format!("plan-{token}.tasks.json"))
    );
    assert_eq!(fs::read(paths.plan()).unwrap(), b"");
    assert_eq!(fs::read(paths.tasks()).unwrap(), b"");
    fs::write(paths.plan(), "keep markdown").unwrap();
    fs::write(paths.tasks(), "keep JSON").unwrap();
    #[cfg(unix)]
    {
        fs::set_permissions(store.plan_directory(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(paths.plan(), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(paths.tasks(), fs::Permissions::from_mode(0o644)).unwrap();
    }
    store.prepare().unwrap();
    assert_eq!(store.paths().unwrap(), paths);
    assert_eq!(fs::read_to_string(paths.plan()).unwrap(), "keep markdown");
    assert_eq!(fs::read_to_string(paths.tasks()).unwrap(), "keep JSON");
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(store.plan_directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for path in [paths.plan(), paths.tasks()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let fresh = PlanArtifactStore::new(fixture.scratch("main"));
    assert!(fresh.paths().is_none());
    fresh.prepare().unwrap();
    assert_ne!(fresh.paths().unwrap(), paths);
    assert_eq!(fs::read_to_string(paths.plan()).unwrap(), "keep markdown");
}

#[test]
fn current_and_legacy_selected_resources_confine_and_isolate_agent_pairs() {
    use parrot_domain::contracts::paths::RootOrigin;
    for origin in [RootOrigin::Current, RootOrigin::Legacy] {
        let fixture = Fixture::with_origin(origin);
        let first = PlanArtifactStore::new(fixture.scratch("first"));
        let second = PlanArtifactStore::new(fixture.scratch("second"));
        first.prepare().unwrap();
        second.prepare().unwrap();
        let first_paths = first.paths().unwrap();
        let second_paths = second.paths().unwrap();
        assert_ne!(first_paths, second_paths);
        for (store, paths) in [(&first, &first_paths), (&second, &second_paths)] {
            assert!(
                store
                    .scratch()
                    .root()
                    .starts_with(fixture.resources.selected_root().path())
            );
            assert_eq!(
                paths.plan().parent().unwrap(),
                store.scratch().plan_directory()
            );
            assert_eq!(
                paths.tasks().parent().unwrap(),
                store.scratch().plan_directory()
            );
            assert_eq!(fs::read_dir(store.plan_directory()).unwrap().count(), 2);
        }
        assert!(!fixture.root.join("workspace/plan").exists());
    }
}

#[test]
fn concurrent_prepare_commits_one_pair_and_one_token() {
    let fixture = Fixture::new();
    let store = Arc::new(PlanArtifactStore::with_tokens(
        fixture.scratch("main"),
        Tokens::fixed(7),
    ));
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let store = store.clone();
            scope.spawn(move || store.prepare().unwrap());
        }
    });
    assert_eq!(fs::read_dir(store.plan_directory()).unwrap().count(), 2);
}

#[test]
fn exclusive_collision_cleanup_removes_only_this_attempts_files() {
    for sibling in ["md", "tasks.json"] {
        let fixture = Fixture::new();
        let scratch = fixture.scratch("main");
        scratch.provision_plan_directory().unwrap();
        let token = "0b".repeat(16);
        let preexisting = scratch
            .plan_directory()
            .join(format!("plan-{token}.{sibling}"));
        fs::write(&preexisting, "not ours").unwrap();
        let store = PlanArtifactStore::with_tokens(scratch, Tokens::fixed(11));
        assert!(store.prepare().is_err());
        assert!(store.paths().is_none());
        assert_eq!(fs::read_to_string(&preexisting).unwrap(), "not ours");
        assert_eq!(fs::read_dir(store.plan_directory()).unwrap().count(), 1);
    }
}

#[test]
fn provisioning_and_reprepare_failures_do_not_invent_a_pair_or_recreate_a_missing_file() {
    let fixture = Fixture::new();
    let scratch = fixture.scratch("main");
    fs::create_dir_all(scratch.plan_directory().parent().unwrap()).unwrap();
    fs::write(scratch.plan_directory(), "not a directory").unwrap();
    let store = PlanArtifactStore::new(scratch);
    assert!(store.prepare().is_err());
    assert!(store.paths().is_none());
    let other = PlanArtifactStore::new(fixture.scratch("other"));
    other.prepare().unwrap();
    let paths = other.paths().unwrap();
    fs::remove_file(paths.tasks()).unwrap();
    assert!(other.prepare().is_err());
    assert_eq!(other.paths().unwrap(), paths);
    assert!(!paths.tasks().exists());
}
