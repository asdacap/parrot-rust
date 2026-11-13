use super::*;
use parrot_domain::contracts::async_ops::BoxFuture;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Stop(AtomicBool);
impl Cancellation for Stop {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            while !self.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    }
}

fn shell(command: &str) -> LaunchSpec {
    let mut launch = LaunchSpec::new("/bin/sh");
    launch.args = vec!["-c".into(), command.into()];
    launch
}

#[tokio::test]
async fn probe_requires_successful_exit_not_just_spawn() {
    let stop = Stop::default();
    execute_probe(&shell("exit 0"), &stop, Duration::from_secs(5))
        .await
        .unwrap();
    let failure = execute_probe(
        &shell("printf 'AppArmor denied' >&2; exit 23"),
        &stop,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert!(failure.to_string().contains("23"));
    assert!(failure.to_string().contains("AppArmor denied"));
}

#[tokio::test]
async fn probe_bounds_stderr_without_blocking_the_child() {
    let failure = execute_probe(
        &shell("head -c 131072 /dev/zero >&2; exit 1"),
        &Stop::default(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_ne!(failure.kind(), io::ErrorKind::TimedOut);
    assert!(failure.to_string().len() < 8192);
}

#[tokio::test]
async fn probe_timeout_and_cancellation_settle_the_child() {
    let stop = Stop::default();
    let marker = std::env::temp_dir().join(format!("parrot-probe-child-{}", std::process::id()));
    let mut launch = shell("printf '%s' $$ > \"$PROBE_PID_FILE\"; exec sleep 60");
    launch
        .environment
        .insert("PROBE_PID_FILE".into(), Some(marker.as_os_str().to_owned()));
    let failure = execute_probe(&launch, &stop, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_reaped(&marker);
    let check = execute_probe(&launch, &stop, Duration::from_secs(5));
    let cancel = async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop.0.store(true, Ordering::Release);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(check, cancel)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    assert_reaped(&marker);
}

fn assert_reaped(marker: &Path) {
    let pid: i32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    std::fs::remove_file(marker).unwrap();
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

#[tokio::test]
async fn cancelled_probe_does_not_start_a_process() {
    let stop = Stop(AtomicBool::new(true));
    let failure = execute_probe(
        &LaunchSpec::new("/not-a-program"),
        &stop,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.kind(), io::ErrorKind::Interrupted);
}

#[cfg(target_os = "linux")]
#[test]
fn missing_sandbox_is_an_error() {
    assert_eq!(
        plan_read(
            &ExecutableLocator::new(std::ffi::OsStr::new("")),
            Path::new("/")
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::Unsupported
    );
}

#[tokio::test]
#[ignore = "requires native sandbox support; run explicitly to record host capability"]
async fn native_probe_reads_directory_with_shell_metacharacters() {
    let directory =
        std::env::temp_dir().join(format!("parrot-read-probe-{} ' ; $", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("entry"), "read me").unwrap();
    let result = probe_sandbox_read(&directory, &Stop::default()).await;
    std::fs::remove_dir_all(&directory).unwrap();
    result.unwrap();
    assert!(
        probe_sandbox_read(&directory, &Stop::default())
            .await
            .is_err()
    );
}
