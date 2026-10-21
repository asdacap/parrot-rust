use parrot_tools::sandbox::macos::{SeatbeltAccess, SeatbeltLaunchPlan, SeatbeltPolicy};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-seatbelt-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, suffix: &str) -> String {
        self.0.join(suffix).to_str().unwrap().to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn actual_source_policy_captures_match_exactly() {
    type Operations<'a> = &'a [(&'a str, &'a str)];
    let cases: &[(&str, Operations<'_>, &str)] = &[
        ("default", &[], include_str!("fixtures/cases/default.sb")),
        (
            "writable-exception",
            &[("AllowWrite", "writable")],
            include_str!("fixtures/cases/writable-exception.sb"),
        ),
        (
            "denial-read-exception",
            &[("DenyRead", "denied"), ("AllowRead", "denied/readable")],
            include_str!("fixtures/cases/denial-read-exception.sb"),
        ),
        (
            "nested-denial-and-exceptions",
            &[
                ("DenyRead", "tree"),
                ("AllowWrite", "tree/open"),
                ("DenyRead", "tree/open/closed"),
                ("AllowRead", "tree/open/closed/readable"),
            ],
            include_str!("fixtures/cases/nested-denial-and-exceptions.sb"),
        ),
        (
            "multiple-write-exceptions",
            &[("AllowWrite", "second"), ("AllowWrite", "first")],
            include_str!("fixtures/cases/multiple-write-exceptions.sb"),
        ),
        (
            "later-ancestor-revokes-child",
            &[("AllowWrite", "tree/child"), ("DenyRead", "tree")],
            include_str!("fixtures/cases/later-ancestor-revokes-child.sb"),
        ),
        (
            "later-child-restores-exception",
            &[("DenyRead", "tree"), ("AllowWrite", "tree/child")],
            include_str!("fixtures/cases/later-child-restores-exception.sb"),
        ),
        (
            "write-revoked-at-same-path",
            &[("AllowWrite", "workspace"), ("DenyWrite", "workspace")],
            include_str!("fixtures/cases/write-revoked-at-same-path.sb"),
        ),
        (
            "read-restored-without-write",
            &[
                ("AllowWrite", "workspace"),
                ("DenyRead", "workspace"),
                ("AllowRead", "workspace"),
            ],
            include_str!("fixtures/cases/read-restored-without-write.sb"),
        ),
        (
            "quotes-and-backslash",
            &[("AllowWrite", "quote\"and\\slash")],
            include_str!("fixtures/cases/quotes-and-backslash.sb"),
        ),
        (
            "utf16-length-and-ordinal-order",
            &[
                ("AllowWrite", "\u{e000}x"),
                ("AllowWrite", "😀"),
                ("AllowWrite", "az"),
                ("AllowWrite", "z"),
            ],
            include_str!("fixtures/cases/utf16-length-and-ordinal-order.sb"),
        ),
        (
            "static-existing-file-and-directory",
            &[("DenyRead", "static-file"), ("DenyRead", "static-dir")],
            include_str!("fixtures/cases/static-existing-file-and-directory.sb"),
        ),
    ];
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("static-file"),
        "ordinary synthetic fixture\n",
    )
    .unwrap();
    fs::create_dir(fixture.0.join("static-dir")).unwrap();
    for (label, operations, expected) in cases {
        let mut policy = SeatbeltPolicy::new();
        for (operation, suffix) in *operations {
            let path = fixture.path(suffix);
            match *operation {
                "AllowRead" => policy.allow_read(&path),
                "AllowWrite" => policy.allow_write(&path),
                "DenyRead" => policy.deny_read(&path),
                "DenyWrite" => policy.deny_write(&path),
                _ => panic!("unexpected operation"),
            }
            .unwrap();
        }
        let actual = policy.capture().unwrap();
        assert_eq!(
            actual.text().replace(fixture.0.to_str().unwrap(), "$ROOT"),
            *expected,
            "{label}"
        );
    }
}

#[test]
fn controls_are_rejected_when_the_filter_is_emitted() {
    for (suffix, source_error) in [
        (
            "control\nname",
            include_str!("fixtures/cases/control-newline-error.error.txt"),
        ),
        (
            "control\u{7f}name",
            include_str!("fixtures/cases/control-del-error.error.txt"),
        ),
    ] {
        let fixture = Fixture::new();
        let mut policy = SeatbeltPolicy::new();
        policy.allow_write(&fixture.path(suffix)).unwrap();
        let error = policy.capture().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let mut source_lines = source_error.lines();
        assert_eq!(
            source_lines.next(),
            Some("System.InvalidOperationException")
        );
        assert_eq!(error.to_string(), source_lines.next().unwrap());
        assert_eq!(source_lines.next(), None);
    }
}

#[test]
fn evaluation_is_last_containing_rule_not_most_specific() {
    let mut policy = SeatbeltPolicy::new();
    policy.allow_write("/workspace/child").unwrap();
    policy.deny_read("/workspace").unwrap();
    assert_eq!(
        policy.evaluate("/workspace/child/file").unwrap(),
        SeatbeltAccess {
            read: false,
            write: false
        }
    );
    policy.allow_read("/workspace/child").unwrap();
    assert_eq!(
        policy.evaluate("/workspace/child/file").unwrap(),
        SeatbeltAccess {
            read: true,
            write: false
        }
    );
    assert_eq!(
        policy.evaluate("/workspace-other/file").unwrap(),
        SeatbeltAccess {
            read: true,
            write: false
        }
    );
}

#[test]
fn lexical_normalization_and_invalid_paths_are_explicit() {
    let mut policy = SeatbeltPolicy::new();
    policy
        .allow_write("/workspace/./child/../target//")
        .unwrap();
    assert!(policy.evaluate("/workspace/target/file").unwrap().write);
    assert!(!policy.evaluate("/workspace/target-sibling").unwrap().write);
    assert!(policy.allow_write("").is_err());
    assert!(policy.allow_read("/nul\0path").is_err());
}

#[test]
fn capture_remains_unchanged_after_rules_and_file_kind_change() {
    let fixture = Fixture::new();
    let path = fixture.path("file");
    fs::write(&path, "synthetic").unwrap();
    let mut policy = SeatbeltPolicy::new();
    policy.deny_read(&path).unwrap();
    let snapshot = policy.capture().unwrap();
    assert!(snapshot.text().contains("(literal \""));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    policy.allow_write(&path).unwrap();
    assert!(snapshot.text().contains("(literal \""));
    assert_ne!(snapshot, policy.capture().unwrap());
}

#[cfg(unix)]
#[test]
fn source_file_exists_semantics_follow_static_symlinks() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fs::write(fixture.0.join("target"), "synthetic").unwrap();
    fs::create_dir(fixture.0.join("directory")).unwrap();
    symlink(fixture.0.join("target"), fixture.0.join("file-link")).unwrap();
    symlink(fixture.0.join("directory"), fixture.0.join("dir-link")).unwrap();
    symlink(fixture.0.join("absent"), fixture.0.join("broken-link")).unwrap();
    for (name, filter) in [
        ("file-link", "literal"),
        ("dir-link", "subpath"),
        ("broken-link", "subpath"),
    ] {
        let mut policy = SeatbeltPolicy::new();
        let path = fixture.path(name);
        policy.deny_read(&path).unwrap();
        assert!(
            policy
                .capture()
                .unwrap()
                .text()
                .contains(&format!("({filter} \"{path}\")"))
        );
    }
}

#[test]
fn launch_plan_uses_profile_and_private_outer_environment() {
    let command = "printf '%s' 'a; $(not-executed) quote\"'";
    let plan = SeatbeltLaunchPlan::create(
        "/usr/bin/sandbox-exec",
        "/private/profile with spaces.sb",
        command,
        "/workspace",
        [("Z", "last"), ("COMMAND_VALUE", "old"), ("A", "first")],
        [("COMMAND_VALUE", "present")],
    );
    assert_eq!(plan.program, PathBuf::from("/usr/bin/sandbox-exec"));
    assert_eq!(plan.current_dir, Some(PathBuf::from("/workspace")));
    assert!(plan.clear_environment);
    assert_eq!(plan.environment.len(), 1);
    assert_eq!(
        plan.environment
            .get(std::ffi::OsStr::new("PATH"))
            .unwrap()
            .as_deref(),
        Some(std::ffi::OsStr::new("/usr/bin:/bin"))
    );
    let args: Vec<_> = plan
        .args
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(
        args,
        [
            "-f",
            "/private/profile with spaces.sb",
            "/usr/bin/env",
            "-i",
            "A=first",
            "COMMAND_VALUE=present",
            "Z=last",
            "/bin/sh",
            "-c",
            command
        ]
    );
    assert_eq!(
        args.join("\n"),
        include_str!("fixtures/cases/launch-arguments.txt").trim_end_matches('\n')
    );
    assert!(!plan.retain_launch_thread);
}

#[test]
fn launch_environment_uses_source_utf16_ordinal_order() {
    let plan = SeatbeltLaunchPlan::create(
        "/usr/bin/sandbox-exec",
        "/private/profile.sb",
        "true",
        "/workspace",
        [("\u{e000}", "private"), ("😀", "supplementary")],
        std::iter::empty::<(&str, &str)>(),
    );
    assert_eq!(plan.args[4], "😀=supplementary");
    assert_eq!(plan.args[5], "\u{e000}=private");
}

#[test]
fn source_policy_does_not_restrict_network_operations() {
    let text = SeatbeltPolicy::new().capture().unwrap();
    assert!(text.text().starts_with("(version 1)\n(allow default)\n"));
    assert!(!text.text().contains("network"));
    assert_eq!(
        text.text(),
        "(version 1)\n(allow default)\n(deny file-write*)\n"
    );
}

#[cfg(unix)]
#[test]
fn compiled_rules_allow_dev_null_by_default() {
    let snapshot =
        SeatbeltPolicy::compile_rules(std::iter::empty::<(&str, SeatbeltAccess)>()).unwrap();
    assert_eq!(
        snapshot.text(),
        "(version 1)\n(allow default)\n(deny file-write* (require-not (literal \"/dev/null\")))\n"
    );
}

#[cfg(unix)]
#[test]
fn compiled_rules_allow_explicit_dev_null_overrides() {
    for (access, expected) in [
        (
            SeatbeltAccess {
                read: true,
                write: false,
            },
            "(version 1)\n(allow default)\n(deny file-write*)\n",
        ),
        (
            SeatbeltAccess {
                read: false,
                write: true,
            },
            "(version 1)\n(allow default)\n(deny file-read* (literal \"/dev/null\"))\n(deny file-write*)\n",
        ),
    ] {
        let snapshot = SeatbeltPolicy::compile_rules([("/dev/null", access)]).unwrap();
        assert_eq!(snapshot.text(), expected);
    }
}

#[cfg(unix)]
#[test]
fn compiled_rules_preserve_later_ancestor_and_child_precedence() {
    let deny = SeatbeltAccess {
        read: false,
        write: false,
    };
    let allow = SeatbeltAccess {
        read: true,
        write: true,
    };
    let restored = SeatbeltPolicy::compile_rules([("/", deny), ("/dev/null", allow)]).unwrap();
    assert_eq!(
        restored.text(),
        "(version 1)\n(allow default)\n(deny file-read* (require-not (literal \"/dev/null\")))\n(deny file-write* (require-not (literal \"/dev/null\")))\n"
    );
    let revoked = SeatbeltPolicy::compile_rules([("/dev/null", allow), ("/", deny)]).unwrap();
    assert_eq!(
        revoked.text(),
        "(version 1)\n(allow default)\n(deny file-read*)\n(deny file-write*)\n"
    );
}

#[cfg(target_os = "macos")]
mod native;
