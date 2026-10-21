//! These are native Seatbelt checks, not Linux policy-string substitutes.
use super::{Fixture, SeatbeltPolicy};
use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

fn run(profile: &str, command: &str) -> std::process::Output {
    assert_eq!(
        std::env::consts::ARCH,
        "aarch64",
        "native release evidence requires Apple Silicon"
    );
    let fixture = Fixture::new();
    let profile_path = fixture.0.join("profile.sb");
    fs::write(&profile_path, profile).unwrap();
    Command::new("/usr/bin/sandbox-exec")
        .args([
            "-f",
            profile_path.to_str().unwrap(),
            "/bin/sh",
            "-c",
            command,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("native Seatbelt must be available; no unsandboxed fallback")
}

#[test]
fn native_policy_enforces_nested_write_denial_and_symlink_target() {
    // Resolve the native temp prefix (macOS /var commonly aliases /private/var)
    // before expressing lexical Seatbelt paths.
    let fixture = Fixture::new();
    let root = fs::canonicalize(&fixture.0).unwrap();
    let allowed = root.join("quote\"and\\slash");
    let denied = root.join("denied");
    fs::create_dir(&allowed).unwrap();
    fs::create_dir(&denied).unwrap();
    symlink(&denied, allowed.join("alias")).unwrap();
    let mut policy = SeatbeltPolicy::new();
    policy.allow_write(root.to_str().unwrap()).unwrap();
    policy.deny_write(denied.to_str().unwrap()).unwrap();
    let text = policy.capture().unwrap();
    let shell_quote =
        |path: &std::path::Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    let allowed_file = allowed.join("ordinary");
    let denied_file = denied.join("blocked");
    let alias_file = allowed.join("alias/blocked");
    let output = run(
        text.text(),
        &format!("printf allowed > {}", shell_quote(&allowed_file)),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&allowed_file).unwrap(), b"allowed");
    for path in [&denied_file, &alias_file] {
        let output = run(
            text.text(),
            &format!("printf forbidden > {}", shell_quote(path)),
        );
        assert!(!output.status.success());
        assert!(!denied_file.exists());
    }
}

#[test]
fn native_default_policy_allows_synthetic_loopback_network() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let text = SeatbeltPolicy::new().capture().unwrap();
    let output = run(
        text.text(),
        &format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_policy_preserves_read_denial_and_child_exception() {
    let fixture = Fixture::new();
    let root = fs::canonicalize(&fixture.0).unwrap();
    let readable = root.join("readable");
    let denied = root.join("denied");
    fs::write(&readable, "allowed").unwrap();
    fs::write(&denied, "blocked").unwrap();
    let mut policy = SeatbeltPolicy::new();
    policy.deny_read(root.to_str().unwrap()).unwrap();
    policy.allow_read(readable.to_str().unwrap()).unwrap();
    let text = policy.capture().unwrap();
    let output = run(text.text(), &format!("/bin/cat '{}'", readable.display()));
    assert!(output.status.success());
    assert_eq!(output.stdout, b"allowed");
    assert!(
        !run(text.text(), &format!("/bin/cat '{}'", denied.display()))
            .status
            .success()
    );
}
