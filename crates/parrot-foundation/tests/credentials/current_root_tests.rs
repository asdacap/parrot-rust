#[cfg(target_os = "linux")]
#[test]
fn current_state_paths_never_read_legacy_config_credentials_or_presets() {
    use super::*;
    use parrot_foundation::paths::StatePaths;
    use std::collections::HashMap;
    use std::process::Command;

    const CHILD_ROOT: &str = "PARROT_T13_READ_WITNESS_ROOT";
    const CONTROL: &str = "PARROT_T13_READ_WITNESS_CONTROL";
    const TEST: &str =
        "current_root_tests::current_state_paths_never_read_legacy_config_credentials_or_presets";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let config_home = root.join("config-home");
        if std::env::var_os(CONTROL).is_some() {
            fs::read(config_home.join("parrotdotnet/credentials.json")).unwrap();
            return;
        }
        let environment = HashMap::from([
            (
                "HOME".to_owned(),
                root.join("home").to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".to_owned(),
                config_home.to_string_lossy().into_owned(),
            ),
        ]);
        let paths = StatePaths::resolve(&environment);
        assert_eq!(paths.config(), config_home.join("parrotrust"));
        assert_eq!(paths.config_file(), paths.config().join("config.yaml"));
        let current = CredentialStore::at_selected_path(paths.credentials_file());
        let presets = CredentialPresets::at_selected_config_directory(paths.config());
        assert_eq!(presets.path(), paths.credential_preset_directory());
        assert!(run(current.list(&NEVER)).unwrap().is_empty());
        assert!(run(current.get("legacy", &NEVER)).unwrap().is_none());
        assert!(run(presets.list(&NEVER)).unwrap().is_empty());
        run(current.set(
            "selected",
            &Credential::for_api_key("FAKE-SELECTED"),
            &NEVER,
        ))
        .unwrap();
        run(presets.save("new", &current, &NEVER)).unwrap();
        run(current.delete("selected", &NEVER)).unwrap();
        run(presets.apply("new", &current, &NEVER)).unwrap();
        drop(current);
        let reopened = CredentialStore::at_selected_path(paths.credentials_file());
        assert_eq!(run(reopened.list(&NEVER)).unwrap(), ["selected"]);
        return;
    }

    let root = root();
    let legacy = root.join("config-home/parrotdotnet");
    let legacy_presets = legacy.join("credential_presets");
    fs::create_dir_all(&legacy_presets).unwrap();
    fs::write(legacy.join("config.yaml"), "not a valid configuration").unwrap();
    fs::write(
        legacy.join("credentials.json"),
        r#"{"version":1,"credentials":{"legacy":{"version":1,"type":"api_key","api_key":{"key":"FAKE-LEGACY"}}}}"#,
    )
    .unwrap();
    fs::write(legacy_presets.join("old.json"), "not a valid preset").unwrap();
    for control in [false, true] {
        let trace = root.join(if control {
            "control.trace"
        } else {
            "selected.trace"
        });
        let mut command = Command::new("strace");
        command
            .args([
                "-f",
                "-yy",
                "-s",
                "4096",
                "-e",
                "trace=%file,read,pread64,getdents64",
                "-o",
            ])
            .arg(&trace)
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1"])
            .env(CHILD_ROOT, &root)
            .env_remove(CONTROL);
        if control {
            command.env(CONTROL, "1");
        }
        let output = command
            .output()
            .expect("actual strace read witness is required");
        assert!(output.status.success(), "{output:?}");
        let observed = fs::read_to_string(trace).unwrap();
        let legacy_path = legacy.to_string_lossy();
        if control {
            assert!(
                observed.lines().any(|line| {
                    line.contains("read(")
                        && line.contains(legacy_path.as_ref())
                        && line.contains("credentials.json")
                }),
                "positive control must show an actual old credential read"
            );
        } else {
            assert!(
                !observed.contains(legacy_path.as_ref()),
                "old-root syscall observed: {observed}"
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}
