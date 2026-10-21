use parrot_tools::process::execution::LaunchSpec;
use parrot_tools::sandbox::linux::{
    LinuxAccessRule, LinuxBubblewrapSandbox, LinuxSandboxPolicy, ReadOnlyExecCommandClassifier,
    UnavailableProcessSandbox, UnsandboxedProcessSandbox,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-linux-sandbox-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.0.join(suffix)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn rule(path: impl Into<PathBuf>, read: bool, write: bool) -> LinuxAccessRule {
    LinuxAccessRule {
        path: path.into(),
        read,
        write,
    }
}

fn strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
}

fn execute(launch: &LaunchSpec) -> Output {
    let mut command = Command::new(&launch.program);
    command.args(&launch.args);
    if let Some(path) = &launch.current_dir {
        command.current_dir(path);
    }
    if launch.clear_environment {
        command.env_clear();
    }
    for (key, value) in &launch.environment {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    // This spawning OS thread stays alive while output waits for the child.
    command.output().unwrap()
}

#[test]
fn pipe_arguments_match_source_containment_and_order() {
    let root = Fixture::new();
    fs::write(root.path("bwrap"), "not executed").unwrap();
    let sandbox = LinuxBubblewrapSandbox::new(&root.path("bwrap"), false).unwrap();
    let policy = LinuxSandboxPolicy {
        device_paths: vec!["/dev/example".into()],
        ..Default::default()
    };
    let launch = sandbox
        .pipe_launch(
            &policy,
            "printf '%s' a; exit 7",
            Path::new("/workspace"),
            &[("EXAMPLE".into(), "a b=c".into())],
        )
        .unwrap();
    assert_eq!(
        strings(launch.args),
        [
            "--die-with-parent",
            "--new-session",
            "--unshare-ipc",
            "--unshare-uts",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--setenv",
            "EXAMPLE",
            "a b=c",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--dev-bind-try",
            "/dev/example",
            "/dev/example",
            "--chdir",
            "/workspace",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s' a; exit 7",
        ]
    );
    assert!(
        !policy
            .arguments("true", Path::new("/"), &[], None)
            .contains(&"--unshare-net".into())
    );
}

#[test]
fn masks_existing_types_and_preserves_later_exceptions_and_owned_parents() {
    let root = Fixture::new();
    fs::create_dir(root.path("denied")).unwrap();
    fs::write(root.path("file"), "secret").unwrap();
    let policy = LinuxSandboxPolicy {
        rules: vec![
            rule(root.path("denied"), false, false),
            rule(root.path("denied/open"), true, true),
            rule(root.path("file"), false, false),
            rule(root.path("missing"), false, false),
        ],
        owned_roots: vec![root.0.clone()],
        ..Default::default()
    };
    let args = strings(policy.arguments("true", Path::new("/"), &[], None));
    let rule_args = &args[9..args.len() - 10];
    let expected = [
        "--dir".to_owned(),
        root.0.display().to_string(),
        "--tmpfs".into(),
        root.path("denied").display().to_string(),
        "--dir".into(),
        root.0.display().to_string(),
        "--dir".into(),
        root.path("denied").display().to_string(),
        "--bind".into(),
        root.path("denied/open").display().to_string(),
        root.path("denied/open").display().to_string(),
        "--dir".into(),
        root.0.display().to_string(),
        "--ro-bind".into(),
        "/dev/null".into(),
        root.path("file").display().to_string(),
        "--dir".into(),
        root.0.display().to_string(),
        "--ro-bind".into(),
        "/dev/null".into(),
        root.path("missing").display().to_string(),
    ];
    assert_eq!(rule_args, expected);
}

#[test]
fn unavailable_is_not_the_explicit_unconfined_adapter() {
    let unavailable = LinuxBubblewrapSandbox::new(Path::new(""), true).unwrap();
    assert!(!unavailable.sandbox_available());
    assert_eq!(
        unavailable
            .pipe_launch(&LinuxSandboxPolicy::default(), "true", Path::new("/"), &[])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(!UnavailableProcessSandbox.sandbox_available());
    assert!(UnavailableProcessSandbox.pipe_launch().is_err());
    assert!(UnsandboxedProcessSandbox.sandbox_available());
    let launch = UnsandboxedProcessSandbox.pipe_launch(
        "printf '%s' \"$T38\"; exit 19",
        Path::new("/"),
        &[("T38".into(), "explicit".into())],
    );
    assert_eq!(launch.program, Path::new("/bin/sh"));
    assert!(!launch.retain_launch_thread);
    let output = execute(&launch);
    assert_eq!(output.status.code(), Some(19));
    assert_eq!(output.stdout, b"explicit");
}

#[test]
fn executable_validation_resolves_symlinks_and_rejects_writable_parent_chain() {
    assert_eq!(
        LinuxBubblewrapSandbox::new(Path::new("bwrap"), false)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let root = Fixture::new();
    assert_eq!(
        LinuxBubblewrapSandbox::new(&root.path("missing"), false)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    fs::write(root.path("bwrap"), "not executed").unwrap();
    assert_eq!(
        LinuxBubblewrapSandbox::new(&root.path("bwrap"), true)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path("bwrap"), root.path("link")).unwrap();
        let sandbox = LinuxBubblewrapSandbox::new(&root.path("link"), false).unwrap();
        let launch = sandbox
            .pipe_launch(&LinuxSandboxPolicy::default(), "true", Path::new("/"), &[])
            .unwrap();
        assert_eq!(launch.program, root.path("bwrap").canonicalize().unwrap());
        assert!(launch.retain_launch_thread);
        assert!(!launch.clear_environment);
        assert!(launch.current_dir.is_none());
        assert!(launch.environment.is_empty());
    }
}

#[test]
fn pty_inner_argv_binds_helper_before_devices_and_attaches_after_chdir() {
    let root = Fixture::new();
    fs::write(root.path("bwrap"), "not executed").unwrap();
    fs::write(root.path("helper"), "not executed").unwrap();
    let sandbox = LinuxBubblewrapSandbox::new(&root.path("bwrap"), false).unwrap();
    let args = strings(
        sandbox
            .pseudo_terminal_arguments(
                &LinuxSandboxPolicy::default(),
                &root.path("helper"),
                "echo hi",
                Path::new("/"),
                &[],
            )
            .unwrap(),
    );
    let executable = root.path("bwrap").canonicalize().unwrap();
    let helper = root.path("helper").display().to_string();
    assert_eq!(
        args,
        [
            executable.to_str().unwrap(),
            "--die-with-parent",
            "--new-session",
            "--unshare-ipc",
            "--unshare-uts",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--ro-bind",
            &helper,
            &helper,
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--chdir",
            "/",
            "--",
            &helper,
            "--attach",
            "--",
            "/bin/sh",
            "-c",
            "echo hi",
        ]
    );
    assert_eq!(
        sandbox
            .pseudo_terminal_arguments(
                &LinuxSandboxPolicy::default(),
                &root.path("missing"),
                "true",
                Path::new("/"),
                &[]
            )
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn classifier_matches_actual_source_cases() {
    let classifier = ReadOnlyExecCommandClassifier::new(vec!["git status".into(), "cat".into()]);
    for (line, expected) in include_str!("fixtures/classifier.tsv")
        .lines()
        .map(|line| line.split_once('\t').unwrap())
    {
        let command = line
            .replace("\\t", "\t")
            .replace("\\n", "\n")
            .replace("\\r", "\r");
        assert_eq!(
            classifier.is_match(&command),
            expected == "true",
            "{command:?}"
        );
    }
    assert!(!ReadOnlyExecCommandClassifier::new(vec![]).is_match(""));
    let empty = ReadOnlyExecCommandClassifier::new(vec![String::new()]);
    assert!(empty.is_match(" \t\n"));
    assert!(!empty.is_match("cat"));
}

#[test]
fn actual_source_mount_observations_match() {
    let root = Fixture::new();
    fs::create_dir(root.path("denied")).unwrap();
    fs::write(root.path("file"), "secret").unwrap();
    // Consume actual effective source inputs; this does not implement or certify
    // Rust security-profile materialization (the separate T36 obligation).
    let rules = include_str!("fixtures/rules.tsv")
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            rule(
                fields[0].replace("$ROOT", root.0.to_str().unwrap()),
                fields[1] == "true",
                fields[2] == "true",
            )
        })
        .collect();
    let policy = LinuxSandboxPolicy {
        rules,
        owned_roots: vec![root.0.clone()],
        ..Default::default()
    };
    let args = strings(policy.arguments("true", Path::new("/"), &[], None));
    let actual = args[9..args.len() - 10]
        .iter()
        .map(|arg| arg.replace(root.0.to_str().unwrap(), "$ROOT"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(actual, include_str!("fixtures/mounts.txt"));
}

#[test]
#[ignore = "requires an actual usable Linux bubblewrap namespace capability; failure is not a skip or unconfined fallback"]
fn native_bwrap_confines_synthetic_roots() {
    let executable = std::env::var_os("T38_BWRAP")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let output = Command::new("sh")
                .args(["-c", "command -v bwrap"])
                .output()
                .unwrap();
            assert!(output.status.success(), "bubblewrap prerequisite missing");
            PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
        });
    let sandbox = LinuxBubblewrapSandbox::new(&executable, false).unwrap();
    let root = Fixture::new();
    fs::create_dir(root.path("writable")).unwrap();
    fs::create_dir(root.path("denied")).unwrap();
    fs::create_dir(root.path("denied/open")).unwrap();
    fs::write(root.path("denied/secret"), "secret").unwrap();
    fs::write(root.path("masked-file"), "secret").unwrap();
    fs::write(root.path("read-only"), "unchanged").unwrap();
    let policy = LinuxSandboxPolicy {
        rules: vec![
            rule(root.path("writable"), true, true),
            rule(root.path("denied"), false, false),
            rule(root.path("denied/open"), true, true),
            rule(root.path("masked-file"), false, false),
        ],
        ..Default::default()
    };
    std::os::unix::fs::symlink("../read-only", root.path("writable/escape")).unwrap();
    std::os::unix::fs::symlink("denied", root.path("denied-alias")).unwrap();
    let network_namespace = fs::read_link("/proc/self/ns/net").unwrap();
    let command = "set -e; test \"$T38_VALUE\" = 'a b=c'; test -r /proc/self/status; test -c /dev/null; test \"$(readlink /proc/self/ns/net)\" = \"$T38_NETWORK_NS\"; test ! -e denied/secret; test ! -e denied-alias/secret; test ! -s masked-file; printf allowed > writable/result; printf exception > denied/open/result; if printf forbidden > read-only; then exit 91; fi; if printf forbidden > writable/../read-only; then exit 92; fi; if printf forbidden > writable/escape; then exit 93; fi; test \"$(cat read-only)\" = unchanged; printf confined";
    let launch = sandbox
        .pipe_launch(
            &policy,
            command,
            &root.0,
            &[
                ("T38_VALUE".into(), "a b=c".into()),
                (
                    "T38_NETWORK_NS".into(),
                    network_namespace.to_str().unwrap().into(),
                ),
            ],
        )
        .unwrap();
    let output = execute(&launch);
    assert!(
        output.status.success(),
        "native confinement failed without downgrade: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"confined");
    assert_eq!(fs::read(root.path("writable/result")).unwrap(), b"allowed");
    assert_eq!(
        fs::read(root.path("denied/open/result")).unwrap(),
        b"exception"
    );
    assert_eq!(fs::read(root.path("read-only")).unwrap(), b"unchanged");
    assert_eq!(fs::read(root.path("masked-file")).unwrap(), b"secret");

    // A real startup failure must not run the command, even though its target is
    // explicitly writable. This is not an unavailable-platform skip.
    let broken = LinuxSandboxPolicy {
        rules: vec![
            rule(root.path("writable"), true, true),
            rule(root.path("missing-mount-source"), true, true),
        ],
        ..Default::default()
    };
    let failed = sandbox
        .pipe_launch(&broken, "printf fallback > writable/fallback", &root.0, &[])
        .unwrap();
    let output = execute(&failed);
    assert!(
        !output.status.success(),
        "missing bind source unexpectedly launched"
    );
    assert!(
        !root.path("writable/fallback").exists(),
        "failed sandbox was downgraded"
    );
}
