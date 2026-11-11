mod regressions;

use parrot_domain::contracts::paths::RootOrigin;
use parrot_domain::values::identifiers::UserSessionId;
use parrot_foundation::paths::{
    AgentScratchDirectory, ApplicationRoots, StatePaths, UserSessionResources,
};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/t10-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn roots(&self) -> ApplicationRoots {
        ApplicationRoots::resolve(&HashMap::from([(
            "HOME".to_owned(),
            self.0.join("home").to_string_lossy().into_owned(),
        )]))
    }
    fn id(&self) -> UserSessionId {
        UserSessionId::parse("session-test").unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn current_and_legacy_roots_use_only_current_home_xdg_without_content_rewrites() {
    for use_xdg in [false, true] {
        let mut environment =
            HashMap::from([("HOME".to_owned(), "/home/parrotdotnet/user".to_owned())]);
        if use_xdg {
            for (key, value) in [
                ("XDG_STATE_HOME", "/xdg/parrotdotnet/state"),
                ("XDG_CONFIG_HOME", "/xdg/config"),
                ("XDG_DATA_HOME", "/xdg/data"),
            ] {
                environment.insert(key.to_owned(), value.to_owned());
            }
        }
        let roots = ApplicationRoots::resolve(&environment);
        let expected_state = if use_xdg {
            "/xdg/parrotdotnet/state"
        } else {
            "/home/parrotdotnet/user/.local/state"
        };
        assert_eq!(
            roots.current().state(),
            Path::new(expected_state).join("parrotrust")
        );
        assert_eq!(
            roots.legacy().state(),
            Path::new(expected_state).join("parrotdotnet")
        );
        assert_eq!(
            roots.current().config_file(),
            roots.current().config().join("config.yaml")
        );
        assert_eq!(
            roots.current().credentials_file(),
            roots.current().config().join("credentials.json")
        );
        assert_eq!(
            roots.current().skills_directory(),
            roots.current().data().join("skills")
        );
    }
}

#[test]
fn state_paths_keep_source_empty_relative_whitespace_ordinal_and_lexical_semantics() {
    let cases = [
        (HashMap::new(), ".local/state/parrotrust"),
        (
            HashMap::from([("HOME".to_owned(), "".to_owned())]),
            ".local/state/parrotrust",
        ),
        (
            HashMap::from([("HOME".to_owned(), "relative".to_owned())]),
            "relative/.local/state/parrotrust",
        ),
        (
            HashMap::from([("HOME".to_owned(), " ".to_owned())]),
            " /.local/state/parrotrust",
        ),
        (
            HashMap::from([
                ("HOME".to_owned(), "/home/x".to_owned()),
                ("XDG_STATE_HOME".to_owned(), "".to_owned()),
            ]),
            "/home/x/.local/state/parrotrust",
        ),
        (
            HashMap::from([("XDG_STATE_HOME".to_owned(), "./a/../b//".to_owned())]),
            "./a/../b//parrotrust",
        ),
        (
            HashMap::from([("XDG_STATE_HOME".to_owned(), " ".to_owned())]),
            " /parrotrust",
        ),
        (
            HashMap::from([("home".to_owned(), "/wrong".to_owned())]),
            ".local/state/parrotrust",
        ),
    ];
    for (environment, expected) in cases {
        assert_eq!(
            StatePaths::resolve(&environment).state().as_os_str(),
            Path::new(expected).as_os_str()
        );
    }
}

#[test]
fn discovery_flag_and_duplicate_id_matrix_selects_one_root() {
    for current_exists in [false, true] {
        for legacy_exists in [false, true] {
            for enabled in [false, true] {
                let fixture = Fixture::new();
                let roots = fixture.roots();
                let id = fixture.id();
                if current_exists {
                    fs::create_dir_all(roots.current().state().join("sessions").join(id.value()))
                        .unwrap();
                }
                if legacy_exists {
                    fs::create_dir_all(roots.legacy().state().join("sessions").join(id.value()))
                        .unwrap();
                }
                let result = roots.discover_session(&id, enabled, &fixture.0).unwrap();
                let expected = if current_exists {
                    Some(RootOrigin::Current)
                } else if legacy_exists && enabled {
                    Some(RootOrigin::Legacy)
                } else {
                    None
                };
                assert_eq!(result.as_ref().map(|root| root.origin()), expected);
                assert_eq!(
                    roots.select_new(&id, &fixture.0).unwrap().origin(),
                    RootOrigin::Current
                );
                if let Some(root) = result {
                    let app = if root.origin() == RootOrigin::Current {
                        "parrotrust"
                    } else {
                        "parrotdotnet"
                    };
                    assert!(
                        root.path()
                            .ends_with(Path::new(app).join("sessions").join(id.value()))
                    );
                }
            }
        }
    }
}

#[test]
fn selected_legacy_resources_never_mix_even_after_flag_changes_or_duplicate_creation() {
    let fixture = Fixture::new();
    let roots = fixture.roots();
    let id = fixture.id();
    let legacy = roots.legacy().state().join("sessions").join(id.value());
    fs::create_dir_all(&legacy).unwrap();
    let root = roots
        .discover_session(&id, true, &fixture.0)
        .unwrap()
        .unwrap();
    let resources = UserSessionResources::new(root.clone(), id.clone(), fixture.0.clone()).unwrap();
    let current = roots.current().state().join("sessions").join(id.value());
    write(&current.join("artifacts/output.txt"), "new duplicate");
    assert_eq!(
        roots
            .discover_session(&id, false, &fixture.0)
            .unwrap()
            .unwrap()
            .origin(),
        RootOrigin::Current
    );
    assert_eq!(resources.root(), legacy);
    assert_eq!(resources.socket_path(), legacy.join("parrot.sock"));
    assert_eq!(resources.scratch_directory(), legacy.join("scratch"));
    assert_eq!(
        root.resolve_internal("artifacts/output.txt")
            .unwrap()
            .path(),
        legacy.join("artifacts/output.txt")
    );
    assert!(!legacy.join("artifacts/output.txt").exists());
    write(&resources.metadata_path(), "legacy write remains in place");
    assert!(!current.join("meta.json").exists());
    let scratch = resources.agent_scratch(&["main", "child"]).unwrap();
    assert!(scratch.root().starts_with(&legacy));
    assert!(scratch.queue_directory().starts_with(&legacy));
}

#[test]
fn present_bad_current_session_does_not_discover_valid_legacy_session() {
    let fixture = Fixture::new();
    let roots = fixture.roots();
    let id = fixture.id();
    let current = roots.current().state().join("sessions").join(id.value());
    fs::create_dir_all(roots.legacy().state().join("sessions").join(id.value())).unwrap();
    write(&current, "not a directory");
    assert_eq!(
        roots
            .discover_session(&id, true, &fixture.0)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    fs::remove_file(&current).unwrap();
    write(&current.join("meta.json"), "malformed metadata");
    let selected = roots
        .discover_session(&id, true, &fixture.0)
        .unwrap()
        .unwrap();
    assert_eq!(selected.origin(), RootOrigin::Current);
    // Storage validates metadata only after this selection; no second root is supplied.
    assert_eq!(
        selected.resolve_internal("meta.json").unwrap().path(),
        current.join("meta.json")
    );
}

#[test]
fn resource_names_are_contained_and_reserved_names_are_only_for_children() {
    let fixture = Fixture::new();
    let root = fixture
        .roots()
        .select_new(&fixture.id(), &fixture.0)
        .unwrap();
    let resources = UserSessionResources::new(root, fixture.id(), fixture.0.clone()).unwrap();
    for invalid in [
        "",
        " ",
        ".",
        "..",
        "../other",
        "..\\other",
        "other/session",
        "other\\session",
        "/rooted",
        "\\rooted",
        "two names",
        "tab\tname",
        "line\nname",
        "a\0b",
    ] {
        assert!(resources.agent_scratch(&["main", invalid]).is_err());
    }
    for valid in [
        "agent-session-child",
        "with.dot",
        "unicode-é",
        "punctuation:!",
    ] {
        assert_eq!(
            resources
                .agent_scratch(&["main", valid])
                .unwrap()
                .queue_directory(),
            resources
                .agents_directory()
                .join("main")
                .join(valid)
                .join("queues")
        );
    }
    assert!(resources.agent_scratch(&[]).is_err());
    for reserved in AgentScratchDirectory::RESERVED_NAMES {
        assert!(resources.agent_scratch(&["main", reserved]).is_err());
        assert!(
            resources
                .agent_scratch(&["main", "worker", reserved])
                .is_err()
        );
        assert_eq!(
            resources.agent_scratch(&[reserved]).unwrap().root(),
            resources.agents_directory().join(reserved)
        );
    }
    let agent = resources.agent_scratch(&["main"]).unwrap();
    let requests = agent.root().join("requests");
    fs::create_dir_all(&requests).unwrap();
    let record = requests.join("request_1.json");
    fs::write(&record, "existing request").unwrap();
    assert!(resources.agent_scratch(&["main", "requests"]).is_err());
    assert_eq!(fs::read(&record).unwrap(), b"existing request");
    let output = agent.root().join("output.txt");
    fs::write(&output, "live agent output").unwrap();
    assert!(resources.agent_scratch(&["main", "output.txt"]).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"live agent output");
    assert!(
        resources
            .selected_root()
            .resolve_internal("../other")
            .is_err()
    );
    assert!(
        resources
            .selected_root()
            .resolve_internal("/absolute")
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn scratch_provisioning_is_private_repeatable_lazy_and_rejects_files_and_final_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let scratch = AgentScratchDirectory::new(&fixture.0.join("agent"), &fixture.0).unwrap();
    for path in [
        scratch.root().to_path_buf(),
        scratch.scratch_path(),
        scratch.blob_directory(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
    }
    assert!(!scratch.history_path().exists());
    assert!(!scratch.plan_directory().exists());
    scratch.provision().unwrap();
    scratch.provision_plan_directory().unwrap();
    for path in [
        scratch.root().to_path_buf(),
        scratch.scratch_path(),
        scratch.blob_directory(),
        scratch.plan_directory(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for leaf in ["root", "scratch", "blobs", "plan"] {
        for is_link in [false, true] {
            let root = fixture.0.join(format!("{leaf}-{is_link}")).join("target");
            let target = if leaf == "root" {
                root.clone()
            } else {
                root.join(leaf)
            };
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            if is_link {
                symlink(scratch.root(), &target).unwrap();
            } else {
                fs::write(&target, "file").unwrap();
            }
            let result = AgentScratchDirectory::new(&root, &fixture.0)
                .and_then(|scratch| scratch.provision_plan_directory());
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }
}

#[cfg(unix)]
#[test]
fn home_aliases_materialize_selected_roots_and_reject_symlinked_session_directories() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let physical = fixture.0.join("physical");
    fs::create_dir_all(&physical).unwrap();
    let alias = fixture.0.join("alias");
    symlink(&physical, &alias).unwrap();
    let roots = ApplicationRoots::resolve(&HashMap::from([(
        "HOME".to_owned(),
        alias.to_string_lossy().into_owned(),
    )]));
    assert!(roots.current().state().starts_with(&alias));
    assert!(
        roots
            .select_new(&fixture.id(), &fixture.0)
            .unwrap()
            .path()
            .starts_with(&physical)
    );
    let session_link = roots
        .current()
        .state()
        .join("sessions")
        .join(fixture.id().value());
    fs::create_dir_all(session_link.parent().unwrap()).unwrap();
    symlink(&physical, &session_link).unwrap();
    assert_eq!(
        roots
            .discover_session(&fixture.id(), true, &fixture.0)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}
