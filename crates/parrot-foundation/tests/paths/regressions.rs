use super::{Fixture, write};
use parrot_domain::contracts::paths::ReadPurpose;
use parrot_foundation::paths::{ApplicationRoots, StatePaths};
use std::collections::HashMap;
use std::path::Path;

#[test]
fn nested_state_and_data_roots_choose_the_most_specific_corresponding_pair() {
    let fixture = Fixture::new();
    for data_inside_state in [false, true] {
        let outer = fixture.0.join(format!("outer-{data_inside_state}"));
        let inner = outer.join("parrotrust/nested");
        let (state, data) = if data_inside_state {
            (&outer, &inner)
        } else {
            (&inner, &outer)
        };
        let roots = ApplicationRoots::resolve(&HashMap::from([
            ("HOME".to_owned(), fixture.0.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".to_owned(),
                state.to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".to_owned(),
                data.to_string_lossy().into_owned(),
            ),
        ]));
        let (current, legacy, purpose) = if data_inside_state {
            (
                roots.current().data(),
                roots.legacy().data(),
                ReadPurpose::ApplicationData,
            )
        } else {
            (
                roots.current().state(),
                roots.legacy().state(),
                ReadPurpose::ApplicationState,
            )
        };
        let current = current.join("skills/example.md");
        let legacy = legacy.join("skills/example.md");
        write(&legacy, "correct nested legacy file");
        for requested in [&current, &legacy] {
            let plan = roots
                .read_candidates(requested, purpose, true, &fixture.0)
                .unwrap();
            assert_eq!(plan.primary_path(), current);
            assert_eq!(plan.fallback_path(), Some(legacy.as_path()));
            assert_eq!(
                plan.read_with(|resolved| std::fs::read_to_string(resolved.actual_path()))
                    .unwrap()
                    .value,
                "correct nested legacy file"
            );
        }
    }
}

#[test]
fn coincident_xdg_homes_still_exclude_known_configuration_and_credentials() {
    let fixture = Fixture::new();
    let root = fixture.0.to_string_lossy().into_owned();
    let roots = ApplicationRoots::resolve(&HashMap::from([
        ("XDG_STATE_HOME".to_owned(), root.clone()),
        ("XDG_CONFIG_HOME".to_owned(), root.clone()),
        ("XDG_DATA_HOME".to_owned(), root),
    ]));
    for path in [
        roots.current().config_file(),
        roots.current().predefined_config_file(),
        roots.current().credentials_file(),
        roots
            .current()
            .credential_preset_directory()
            .join("work.json"),
    ] {
        assert!(
            roots
                .read_candidates(&path, ReadPurpose::ApplicationState, true, &fixture.0)
                .unwrap()
                .fallback_path()
                .is_none()
        );
    }
    assert!(
        roots
            .read_candidates(
                &roots.current().state().join("history.txt"),
                ReadPurpose::ApplicationState,
                true,
                &fixture.0
            )
            .unwrap()
            .fallback_path()
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_does_not_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;
    if std::env::var_os("T10_ENV_CHILD").is_some() {
        assert_eq!(
            StatePaths::resolve_from_environment().state(),
            Path::new("/synthetic/home/.local/state/parrotrust")
        );
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "regressions::non_unicode_environment_does_not_panic",
            "--nocapture",
        ])
        .env("T10_ENV_CHILD", "1")
        .env("HOME", "/synthetic/home")
        .env("UNRELATED_NON_UNICODE", OsString::from_vec(vec![0xff]))
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[cfg(unix)]
#[test]
fn selected_physical_root_does_not_move_when_home_alias_changes() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let first = fixture.0.join("first");
    let second = fixture.0.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let alias = fixture.0.join("home-alias");
    symlink(&first, &alias).unwrap();
    let roots = ApplicationRoots::resolve(&HashMap::from([(
        "HOME".to_owned(),
        alias.to_string_lossy().into_owned(),
    )]));
    let selected = roots.select_new(&fixture.id(), &fixture.0).unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();
    assert!(selected.path().starts_with(&first));
    assert!(
        roots
            .select_new(&fixture.id(), &fixture.0)
            .unwrap()
            .path()
            .starts_with(&second)
    );
    assert!(
        selected
            .resolve_internal("session.db")
            .unwrap()
            .path()
            .starts_with(&first)
    );
}
