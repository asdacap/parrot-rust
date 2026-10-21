//! Native counterparts of ProcessRunner, ExecutableLocator and PTY lifecycle predicates.
//! These tests intentionally exercise unsandboxed native plumbing, not authorization.

use super::*;
use crate::process::execution::locator::ExecutableLocator;
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shell(script: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), script.into()];
    spec
}

fn drain(mut reader: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    })
}

async fn wait(process: &mut NativeProcess) -> ExitStatus {
    tokio::time::timeout(Duration::from_secs(10), process.wait())
        .await
        .unwrap()
        .unwrap()
}

#[test]
fn locator_preserves_first_executable_order_and_ignores_empty_path_entries() {
    let directory = Directory::new();
    let first = directory.0.join("first");
    let second = directory.0.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    for (root, mode) in [(&first, 0o600), (&second, 0o700)] {
        std::fs::write(root.join("utility"), "").unwrap();
        std::fs::set_permissions(root.join("utility"), std::fs::Permissions::from_mode(mode))
            .unwrap();
    }
    let path = std::env::join_paths([&first, &second]).unwrap();
    let locator = ExecutableLocator::new(&path);
    assert_eq!(locator.locate("utility"), Some(second.join("utility")));
    assert_eq!(locator.locate(first.join("utility")), None);
    assert_eq!(
        locator.locate(second.join("utility")),
        Some(second.join("utility"))
    );
    assert_eq!(
        ExecutableLocator::new(OsStr::new("")).locate("Cargo.toml"),
        None
    );
    assert_eq!(
        ExecutableLocator::new(OsStr::new(" : \t :")).locate("Cargo.toml"),
        None
    );
    assert_eq!(locator.locate("not-found"), None);
    // Directories with execute/search bits are not executable files.
    assert_eq!(locator.locate(first), None);
}

#[tokio::test]
async fn pipe_captures_both_streams_and_caches_exit_status() {
    let mut process =
        NativeProcess::start_pipe(&shell("printf stdout; printf stderr >&2; exit 7")).unwrap();
    let stdout = drain(process.take_stdout().unwrap());
    let stderr = drain(process.take_stderr().unwrap());
    let status = wait(&mut process).await;
    assert_eq!(status.code(), Some(7));
    assert_eq!(process.wait().await.unwrap(), status);
    assert_eq!(stdout.join().unwrap(), b"stdout");
    assert_eq!(stderr.join().unwrap(), b"stderr");
    assert!(!process.is_pseudo_terminal());
    assert_eq!(
        process.write_stdin(b"input").await.unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        process.resize(24, 80).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        process
            .send_signal(libc::SIGTERM)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn pipe_preserves_working_directory_and_exact_environment_edits() {
    let directory = Directory::new();
    let mut spec = shell("printf '%s|%s|%s' \"$PWD\" \"${ADDED-unset}\" \"${REMOVED-unset}\"");
    spec.current_dir = Some(directory.0.clone());
    spec.clear_environment = true;
    spec.environment
        .insert("ADDED".into(), Some("actual value".into()));
    spec.environment.insert("REMOVED".into(), None);
    let mut process = NativeProcess::start_pipe(&spec).unwrap();
    let stdout = drain(process.take_stdout().unwrap());
    let stderr = drain(process.take_stderr().unwrap());
    assert!(wait(&mut process).await.success());
    assert_eq!(
        stdout.join().unwrap(),
        format!("{}|actual value|unset", directory.0.display()).into_bytes()
    );
    assert!(stderr.join().unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_pipe_drains_survive_sustained_output() {
    let mut process = NativeProcess::start_pipe(&shell("i=0; while [ $i -lt 16384 ]; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; printf 'fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210' >&2; i=$((i+1)); done")).unwrap();
    let stdout = drain(process.take_stdout().unwrap());
    let stderr = drain(process.take_stderr().unwrap());
    assert!(wait(&mut process).await.success());
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    assert_eq!(stdout.len(), 1 << 20);
    assert_eq!(stderr.len(), 1 << 20);
    assert!(
        stdout
            .chunks_exact(64)
            .all(|chunk| chunk
                == b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
    );
    assert!(
        stderr
            .chunks_exact(64)
            .all(|chunk| chunk
                == b"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
    );
}

#[tokio::test]
async fn explicit_signal_targets_the_owned_child_and_wait_is_joined() {
    let mut process = NativeProcess::start_pipe(&shell("exec sleep 30")).unwrap();
    let pid = process.id();
    let stdout = drain(process.take_stdout().unwrap());
    let stderr = drain(process.take_stderr().unwrap());
    process.send_signal(libc::SIGTERM).unwrap();
    assert_eq!(wait(&mut process).await.signal(), Some(libc::SIGTERM));
    stdout.join().unwrap();
    stderr.join().unwrap();
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn cancel_kills_background_group_and_releases_inherited_pipe_handles() {
    let mut process = NativeProcess::start_pipe(&shell("sleep 30 & printf ready; wait")).unwrap();
    let pid = process.id();
    let mut stdout = process.take_stdout().unwrap();
    let mut ready = [0; 5];
    stdout.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let stdout = drain(stdout);
    let stderr = drain(process.take_stderr().unwrap());
    let result = tokio::time::timeout(Duration::from_secs(5), process.cancel_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.signal(), Some(libc::SIGKILL));
    assert_eq!(process.wait().await.unwrap(), result);
    assert!(stdout.join().unwrap().is_empty());
    assert!(stderr.join().unwrap().is_empty());
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn dropping_wait_observer_does_not_detach_process_and_drop_reaps() {
    let mut process = NativeProcess::start_pipe(&shell("sleep 30 & printf ready; wait")).unwrap();
    let pid = process.id();
    let mut stdout = process.take_stdout().unwrap();
    stdout.read_exact(&mut [0; 5]).unwrap();
    let stdout = drain(stdout);
    let stderr = drain(process.take_stderr().unwrap());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.wait())
            .await
            .is_err()
    );
    drop(process);
    stdout.join().unwrap();
    stderr.join().unwrap();
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn retained_launch_thread_is_joined_after_failure_and_success() {
    let mut missing = LaunchSpec::new("/parrot-native-does-not-exist");
    missing.retain_launch_thread = true;
    assert_eq!(
        NativeProcess::start_pipe(&missing).err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    let mut spec = shell("printf retained");
    spec.retain_launch_thread = true;
    let mut process = NativeProcess::start_pipe(&spec).unwrap();
    let stdout = drain(process.take_stdout().unwrap());
    let stderr = drain(process.take_stderr().unwrap());
    assert!(wait(&mut process).await.success());
    assert!(process.launcher.is_none());
    assert_eq!(stdout.join().unwrap(), b"retained");
    assert!(stderr.join().unwrap().is_empty());
    let mut spec = shell("exec sleep 30");
    spec.retain_launch_thread = true;
    let process = NativeProcess::start_pipe(&spec).unwrap();
    let pid = process.id();
    drop(process);
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[cfg(target_os = "linux")]
fn helper() -> &'static Path {
    Path::new(env!("PARROT_PTY_HELPER"))
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn retained_real_helper_provides_terminal_stdin_resize_and_eof() {
    let script = "test -t 0 && test -t 1 && test -t 2 || exit 91; stty -echo; printf ready; read value; stty size; printf '<%s>' \"$value\"; read rest; printf '<EOF>'";
    let mut process = NativeProcess::start_pty(&shell(script), helper(), 24, 80)
        .await
        .unwrap();
    assert!(process.is_pseudo_terminal());
    assert!(process.take_stdout().is_none());
    let mut reader = process.take_pty_reader().unwrap();
    let mut ready = [0; 5];
    reader.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let reader = drain(reader);
    let stderr = drain(process.take_stderr().unwrap());
    process.resize(40, 120).unwrap();
    process.write_stdin("hello 界\n".as_bytes()).await.unwrap();
    // EOT at an empty canonical line gives read(2) EOF without closing the drain.
    process.close_stdin().await.unwrap();
    assert!(wait(&mut process).await.success());
    let output = String::from_utf8(reader.join().unwrap()).unwrap();
    assert_eq!(output, "40 120\r\n<hello 界><EOF>");
    assert!(stderr.join().unwrap().is_empty());
    assert_eq!(
        process.write_stdin(b"later").await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pty_cancellation_releases_terminal_drains_and_reaps_bridge() {
    let mut process =
        NativeProcess::start_pty(&shell("printf ready; sleep 30 & wait"), helper(), 24, 80)
            .await
            .unwrap();
    let pid = process.id();
    let mut reader = process.take_pty_reader().unwrap();
    let mut ready = [0; 5];
    reader.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let reader = drain(reader);
    let stderr = drain(process.take_stderr().unwrap());
    let status = tokio::time::timeout(Duration::from_secs(5), process.cancel_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(reader.join().unwrap().is_empty());
    assert!(stderr.join().unwrap().is_empty());
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn helper_rejects_non_terminal_attach_and_missing_helper_is_not_fallback() {
    let result = Command::new(helper())
        .args(["--attach", "--", "/bin/sh", "-c", "exit 0"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("validate tty"));
    assert!(
        NativeProcess::start_pty(
            &shell("exit 0"),
            Path::new("/parrot-missing-helper"),
            24,
            80
        )
        .await
        .is_err()
    );
    // A failing bridge must be reaped, with no attempted unsandboxed command rerun.
    let directory = Directory::new();
    let marker = directory.0.join("should-not-exist");
    let mut file = File::create(directory.0.join("invalid-helper")).unwrap();
    file.write_all(b"not an executable").unwrap();
    assert!(
        NativeProcess::start_pty(
            &shell(&format!("touch '{}'", marker.display())),
            &directory.0.join("invalid-helper"),
            24,
            80
        )
        .await
        .is_err()
    );
    assert!(!marker.exists());
}

#[test]
fn utility_availability_sorts_classifies_and_copies_inputs() {
    use crate::process::execution::locator::CliUtilityAvailability;
    let directory = Directory::new();
    for name in ["expected-two", "expected-one", "optional-two"] {
        let file = directory.0.join(name);
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let locator = ExecutableLocator::new(directory.0.as_os_str());
    let mut expected: Vec<String> = ["expected-two", "missing-two", "expected-one", "missing-one"]
        .map(String::from)
        .into();
    let mut optional: Vec<String> = ["missing-optional", "optional-two"]
        .map(String::from)
        .into();
    let availability = CliUtilityAvailability::inspect(&expected, &optional, &locator);
    expected.clear();
    optional.clear();
    assert_eq!(
        availability.available_expected(),
        ["expected-one", "expected-two"]
    );
    assert_eq!(
        availability.missing_expected(),
        ["missing-one", "missing-two"]
    );
    assert_eq!(availability.available_optional(), ["optional-two"]);
    let non_bmp =
        CliUtilityAvailability::inspect(&["\u{e000}".into(), "\u{10000}".into()], &[], &locator);
    assert_eq!(non_bmp.missing_expected(), ["\u{10000}", "\u{e000}"]);
}

#[cfg(target_os = "linux")]
fn executable_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/process/execution/fixtures")
        .join(name)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_readiness_reaps_a_successfully_started_stalled_helper() {
    let directory = Directory::new();
    let marker = directory.0.join("helper-pid");
    let stalled = executable_fixture("stalled-helper.sh");
    let mut spec = shell("exit 0");
    spec.environment.insert(
        "PARROT_TEST_HELPER_PID_PATH".into(),
        Some(marker.clone().into_os_string()),
    );
    let mut start = Box::pin(NativeProcess::start_pty(&spec, &stalled, 24, 80));
    let marker_ready = async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        _ = marker_ready => {},
        result = &mut start => panic!("stalled helper unexpectedly completed: {:?}", result.err().map(|error| (error.kind(), error.raw_os_error(), error.to_string()))),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("stalled helper never started"),
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut start)
            .await
            .is_err()
    );
    let pid: i32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    drop(start);
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn invalid_readiness_kills_and_reaps_started_helper_without_command_fallback() {
    let directory = Directory::new();
    let marker = directory.0.join("helper-pid");
    let invalid = executable_fixture("invalid-ready-helper.sh");
    let mut spec = shell("exit 0");
    spec.environment.insert(
        "PARROT_TEST_HELPER_PID_PATH".into(),
        Some(marker.clone().into_os_string()),
    );
    let error = NativeProcess::start_pty(&spec, &invalid, 24, 80)
        .await
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("NOTREADY"),
        "unexpected readiness error: kind={:?}, raw={:?}, message={error}",
        error.kind(),
        error.raw_os_error()
    );
    let pid: i32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn blocked_nonblocking_pty_write_can_be_cancelled_then_process_settled() {
    let mut process = NativeProcess::start_pty(
        &shell("stty -echo -icanon min 1 time 0; printf ready; exec sleep 30"),
        helper(),
        24,
        80,
    )
    .await
    .unwrap();
    let mut reader = process.take_pty_reader().unwrap();
    let mut ready = [0; 5];
    reader.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let reader = drain(reader);
    let stderr = drain(process.take_stderr().unwrap());
    let bytes = vec![b'x'; 4 << 20];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.write_stdin(&bytes))
            .await
            .is_err()
    );
    let status = tokio::time::timeout(Duration::from_secs(5), process.cancel_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(reader.join().unwrap().is_empty());
    assert!(stderr.join().unwrap().is_empty());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_targets_the_terminal_foreground_group_distinct_from_bridge() {
    let mut process = NativeProcess::start_pty(
        &shell("printf '%s ' \"$$\"; exec sleep 30"),
        helper(),
        24,
        80,
    )
    .await
    .unwrap();
    let mut reader = process.take_pty_reader().unwrap();
    let mut digits = Vec::new();
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        if byte[0] == b' ' {
            break;
        }
        digits.push(byte[0]);
    }
    let foreground: i32 = String::from_utf8(digits).unwrap().parse().unwrap();
    assert_ne!(foreground, process.id() as i32);
    let mut actual: libc::pid_t = 0;
    assert_eq!(
        unsafe {
            libc::ioctl(
                process.master.as_ref().unwrap().as_raw_fd(),
                libc::TIOCGPGRP,
                &mut actual,
            )
        },
        0
    );
    assert_eq!(actual, foreground);
    let reader = drain(reader);
    let stderr = drain(process.take_stderr().unwrap());
    assert_eq!(
        process.cancel_and_wait().await.unwrap().signal(),
        Some(libc::SIGKILL)
    );
    assert!(reader.join().unwrap().is_empty());
    assert!(stderr.join().unwrap().is_empty());
    // Reparented helper children may briefly remain zombies until init reaps
    // them; no live foreground process may survive cancellation.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{foreground}/stat")) {
        assert_eq!(stat.rsplit_once(") ").unwrap().1.chars().next(), Some('Z'));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn pty_descriptors_are_atomically_cloexec_during_concurrent_child_launch() {
    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let allocating = active.clone();
    let thread = std::thread::spawn(move || {
        while allocating.load(Ordering::Relaxed) {
            let (master, slave) = open_pty(24, 80).unwrap();
            for file in [&master, &slave] {
                assert_ne!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
            }
        }
    });
    let mut failure = None;
    for _ in 0..64 {
        let child = Command::new("/bin/sh")
            .args(["-c", "for fd in /proc/self/fd/*; do readlink \"$fd\"; done"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&child.stdout).contains("/dev/pts/") {
            failure = Some(child.stdout);
            break;
        }
    }
    active.store(false, Ordering::Relaxed);
    thread.join().unwrap();
    assert!(
        failure.is_none(),
        "a spawned child inherited a PTY descriptor: {failure:?}"
    );
}
