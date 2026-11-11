use super::Fixture;
use parrot_foundation::paths::{ApplicationRoots, StatePaths};
use std::collections::HashMap;
use std::path::Path;

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
