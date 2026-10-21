//! Source-shaped independent ShellProcessExecution predicates and lifetime regressions.
use super::*;
use std::fs;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-composition-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[cfg(target_os = "linux")]
struct Holder(Child);
#[cfg(target_os = "linux")]
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn shell(script: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new("/bin/sh");
    spec.args = vec!["-c".into(), script.into()];
    spec
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(TICK).await;
        }
    })
    .await
    .unwrap();
}
async fn result(execution: &ShellProcessExecution) -> ProcessResult {
    tokio::time::timeout(Duration::from_secs(10), execution.wait())
        .await
        .unwrap()
        .unwrap()
}
fn reaped(pid: u32) {
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
fn no_temporary_files(directory: &Path) {
    assert!(fs::read_dir(directory).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".process-")
    }));
}
#[cfg(target_os = "linux")]
fn helper() -> &'static Path {
    Path::new(env!("PARROT_PTY_HELPER"))
}
#[cfg(target_os = "linux")]
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/process/execution/fixtures")
        .join(name)
}

#[tokio::test]
async fn pipe_eager_live_files_cached_result_and_concurrent_waiters() {
    let directory = Directory::new();
    let execution = Arc::new(
        ShellProcessExecution::start_pipe(
            &shell("printf live; sleep 0.1; printf end; printf error >&2; exit 7"),
            &directory.0,
        )
        .unwrap(),
    );
    assert!(!execution.is_pseudo_terminal());
    let stdout = execution.stdout_path().unwrap();
    let stderr = execution.stderr_path().unwrap();
    assert!(stdout.is_file() && stderr.is_file());
    assert_eq!(
        execution.read_result(0).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    until(|| fs::read(&stdout).unwrap().starts_with(b"live")).await;
    let (_, paths) = execution.read_transcript(0).unwrap();
    assert_eq!(
        String::from_utf16(&paths).unwrap(),
        format!(
            "stdout: {}\nstderr: {}\n",
            stdout.display(),
            stderr.display()
        )
    );
    assert!(execution.read_transcript(1).is_err());
    let a = execution.clone();
    let b = execution.clone();
    let a = tokio::spawn(async move { a.wait().await.unwrap() });
    let b = tokio::spawn(async move { b.wait().await.unwrap() });
    let one = a.await.unwrap();
    assert_eq!(one, b.await.unwrap());
    assert_eq!(one.exit_code, 7);
    assert_eq!(one.stdout, "liveend");
    assert_eq!(one.stderr, "error");
    assert!(one.blob_path.is_none());
    assert!(execution.is_completed());
    assert_eq!(execution.read_result(0).unwrap(), (0, one.clone()));
    assert_eq!(execution.try_result().unwrap(), Some(one));
    assert!(execution.write_stdin("x").await.is_err());
    execution.settle().await.unwrap();
    execution.settle().await.unwrap();
    assert_eq!(fs::read(&stdout).unwrap(), b"liveend");
    assert_eq!(fs::read(&stderr).unwrap(), b"error");
    reaped(execution.id());
}

// Original ProcessRunner oversized and combined UTF-8 formatting operands.
#[tokio::test]
async fn original_oversized_and_combined_utf8_results_spill_without_loss() {
    for (script, expected_out, expected_err, exit) in [
        (
            "awk 'BEGIN{for(i=0;i<70000;i++)printf \"o\"}'; awk 'BEGIN{for(i=0;i<70000;i++)printf \"e\"}' >&2; exit 7",
            "o".repeat(70000),
            "e".repeat(70000),
            7,
        ),
        (
            "awk 'BEGIN{for(i=0;i<11000;i++)printf \"€\"}'; awk 'BEGIN{for(i=0;i<11000;i++)printf \"€\"}' >&2",
            "€".repeat(11000),
            "€".repeat(11000),
            0,
        ),
    ] {
        let directory = Directory::new();
        let execution = ShellProcessExecution::start_pipe(&shell(script), &directory.0).unwrap();
        let actual = result(&execution).await;
        assert_eq!(actual.exit_code, exit);
        assert!(actual.stdout.is_empty() && actual.stderr.is_empty());
        let blob = actual.blob_path.as_ref().unwrap();
        assert!(blob.is_absolute() && blob.starts_with(&directory.0));
        assert!(blob.to_str().unwrap().ends_with("-arse.dat"));
        assert_eq!(
            fs::read_to_string(blob).unwrap(),
            format!(
                "{}\n[stdout]\n{}\n[stderr]\n{}",
                output::format_completion(exit, actual.elapsed_milliseconds),
                expected_out,
                expected_err
            )
        );
        assert_eq!(
            fs::read_to_string(execution.stdout_path().unwrap()).unwrap(),
            expected_out
        );
        assert_eq!(
            fs::read_to_string(execution.stderr_path().unwrap()).unwrap(),
            expected_err
        );
        no_temporary_files(&directory.0);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn private_spill_is_independent_of_overwritten_or_unlinked_live_output() {
    for unlink in [false, true] {
        let directory = Directory::new();
        let execution = ShellProcessExecution::start_pipe(
            &shell("awk 'BEGIN{for(i=0;i<70000;i++)printf \"o\"}'; kill -STOP $$; printf suffix"),
            &directory.0,
        )
        .unwrap();
        let live = execution.stdout_path().unwrap();
        until(|| {
            fs::metadata(&live).is_ok_and(|metadata| metadata.len() >= 70000)
                && fs::read_to_string(format!("/proc/{}/status", execution.id())).is_ok_and(
                    |text| {
                        text.lines()
                            .any(|line| line.starts_with("State:") && line.contains('T'))
                    },
                )
        })
        .await;
        if unlink {
            fs::remove_file(&live).unwrap();
        } else {
            fs::write(&live, "caller replacement").unwrap();
        }
        execution.send_signal(libc::SIGCONT).unwrap();
        let actual = result(&execution).await;
        let blob = fs::read_to_string(actual.blob_path.as_ref().unwrap()).unwrap();
        assert_eq!(
            blob,
            format!(
                "{}\n[stdout]\n{}suffix",
                output::format_completion(0, actual.elapsed_milliseconds),
                "o".repeat(70000)
            )
        );
        execution.settle().await.unwrap();
        no_temporary_files(&directory.0);
    }
}

#[tokio::test]
async fn simultaneous_pressure_and_utf8_stream_boundaries_are_preserved() {
    let directory = Directory::new();
    let script = "awk 'BEGIN{for(i=0;i<20000;i++)printf \"界😀\"}' & awk 'BEGIN{for(i=0;i<20000;i++)printf \"e\"}' >&2 & wait";
    let execution = ShellProcessExecution::start_pipe(&shell(script), &directory.0).unwrap();
    let actual = result(&execution).await;
    assert_eq!(actual.exit_code, 0);
    let blob = fs::read_to_string(actual.blob_path.as_ref().unwrap()).unwrap();
    assert_eq!(
        blob,
        format!(
            "{}\n[stdout]\n{}\n[stderr]\n{}",
            output::format_completion(0, actual.elapsed_milliseconds),
            "界😀".repeat(20000),
            "e".repeat(20000)
        )
    );
}

#[tokio::test]
async fn completion_progresses_without_observers_and_does_not_time_observer_delay() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pipe(&shell("printf done"), &directory.0).unwrap();
    until(|| recover(&execution.shared.exited).is_some()).await;
    let elapsed = recover(&execution.shared.exited).unwrap().1;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(execution.is_completed());
    assert_eq!(
        execution
            .try_result()
            .unwrap()
            .unwrap()
            .elapsed_milliseconds,
        elapsed
    );
    assert_eq!(result(&execution).await.stdout, "done");
}

#[cfg(target_os = "linux")]
async fn inherited_pipe_case(directory: &Directory) -> (ShellProcessExecution, Holder) {
    let execution =
        ShellProcessExecution::start_pipe(&shell("kill -STOP $$; printf done"), &directory.0)
            .unwrap();
    until(|| {
        fs::read_to_string(format!("/proc/{}/status", execution.id())).is_ok_and(|text| {
            text.lines()
                .any(|line| line.starts_with("State:") && line.contains('T'))
        })
    })
    .await;
    // A distinct test-owned child holds a duplicate writer; it can be killed and
    // directly reaped by this test, independently of the execution owner.
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{}/fd/1", execution.id()))
        .unwrap();
    let holder = Holder(
        Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    execution.send_signal(libc::SIGCONT).unwrap();
    until(|| recover(&execution.shared.exited).is_some()).await;
    (execution, holder)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_exit_waits_for_output_eof_but_cancel_and_drop_join_inherited_drains() {
    let directory = Directory::new();
    let (execution, holder) = inherited_pipe_case(&directory).await;
    let elapsed = recover(&execution.shared.exited).unwrap().1;
    assert!(
        tokio::time::timeout(Duration::from_millis(60), execution.wait())
            .await
            .is_err()
    );
    assert!(!execution.is_completed());
    assert_eq!(execution.try_result().unwrap(), None);
    drop(holder);
    let actual = result(&execution).await;
    assert_eq!(actual.stdout, "done");
    assert_eq!(actual.elapsed_milliseconds, elapsed);
    let (execution, holder) = inherited_pipe_case(&directory).await;
    execution.request_cancel();
    tokio::time::timeout(Duration::from_secs(2), execution.settle())
        .await
        .unwrap()
        .unwrap();
    assert!(execution.is_completed());
    assert_eq!(
        execution.wait().await.unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(recover(&execution.state).drains.is_empty());
    reaped(execution.id());
    drop(holder);
    let (execution, holder) = inherited_pipe_case(&directory).await;
    let pid = execution.id();
    let shared = execution.shared.clone();
    let state = execution.state.clone();
    drop(execution);
    assert!(recover(&shared.native).is_none());
    assert!(recover(&state).drains.is_empty());
    reaped(pid);
    drop(holder);
}

#[tokio::test]
async fn wait_observer_cancellation_does_not_detach_and_explicit_cancel_is_not_success() {
    let directory = Directory::new();
    let execution =
        ShellProcessExecution::start_pipe(&shell("printf ready; exec sleep 30"), &directory.0)
            .unwrap();
    until(|| fs::read(execution.stdout_path().unwrap()).unwrap() == b"ready").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), execution.wait())
            .await
            .is_err()
    );
    assert!(!execution.is_completed());
    execution.request_cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), execution.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(
        execution.try_result().unwrap_err().to_string(),
        error.to_string()
    );
    execution.settle().await.unwrap();
    reaped(execution.id());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_preserves_payload_written_before_marker_in_live_output() {
    for (ascii, suffix, expected) in [
        (8192, "", "t".repeat(8192)),
        // Exactly 8192 sent bytes: valid Euro/emoji plus an incomplete final
        // UTF-8 sequence. Finite cancellation-drain completion must flush it.
        (
            8183,
            r"printf '\342\202\254\360\237\230\200\342\202';",
            format!("{}€😀\u{fffd}", "t".repeat(8183)),
        ),
    ] {
        let directory = Directory::new();
        let marker = directory.0.join("written");
        let script = format!(
            "kill -STOP $$; awk 'BEGIN{{for(i=0;i<{ascii};i++)printf \"t\"}}'; {suffix} printf written > '{}'; exec sleep 30",
            marker.display()
        );
        let execution = ShellProcessExecution::start_pipe(&shell(&script), &directory.0).unwrap();
        until(|| {
            fs::read_to_string(format!("/proc/{}/status", execution.id())).is_ok_and(|text| {
                text.lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'))
            })
        })
        .await;
        // Hold the real live writer while the child completes its bounded
        // write, ensuring queued data remains when cancellation is requested.
        let files = execution.files.as_ref().unwrap().clone();
        let (locked, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = thread::spawn(move || {
            let _guard = recover(&files);
            locked.send(()).unwrap();
            released.recv().unwrap();
        });
        ready.recv().unwrap();
        execution.send_signal(libc::SIGCONT).unwrap();
        until(|| marker.exists()).await;
        execution.request_cancel();
        release.send(()).unwrap();
        holder.join().unwrap();
        execution.settle().await.unwrap();
        assert_eq!(
            execution.wait().await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            fs::read(execution.stdout_path().unwrap()).unwrap(),
            expected.as_bytes()
        );
        assert!(recover(&execution.state).drains.is_empty());
        reaped(execution.id());
        no_temporary_files(&directory.0);
    }
}

#[tokio::test]
async fn startup_failure_deletes_only_new_output_files() {
    let directory = Directory::new();
    fs::write(directory.0.join("prior"), "untouched").unwrap();
    assert!(
        ShellProcessExecution::start_pipe(
            &LaunchSpec::new("/parrot-missing-executable"),
            &directory.0
        )
        .is_err()
    );
    let entries: Vec<_> = fs::read_dir(&directory.0)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["prior"]);
    assert_eq!(
        fs::read_to_string(directory.0.join("prior")).unwrap(),
        "untouched"
    );
}

#[tokio::test]
async fn actual_stream_failure_preserves_first_error_and_settles_other_work() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pipe(
        &shell("sleep 0.05; printf failure; exec sleep 30"),
        &directory.0,
    )
    .unwrap();
    recover(execution.files.as_ref().unwrap())
        .complete_stdout()
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), execution.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert_ne!(first.kind(), io::ErrorKind::Interrupted);
    assert_eq!(execution.try_result().unwrap_err().kind(), first.kind());
    assert_eq!(
        execution.try_result().unwrap_err().to_string(),
        first.to_string()
    );
    execution.settle().await.unwrap();
    execution.settle().await.unwrap();
    assert!(recover(&execution.state).drains.is_empty());
    reaped(execution.id());
}

// Original ShellProcessOwnersTests.Faulted_process_completion: moving the
// output directory must fault the result with logical blob context, while
// disposing the owner still joins successfully rather than replaying the fault.
#[tokio::test]
async fn faulted_blob_capture_retains_context_but_settlement_does_not_rethrow_work() {
    let directory = Directory::new();
    let blob = directory.0.join("blob");
    let marker = directory.0.join("produce");
    let script = format!(
        "while [ ! -f '{}' ]; do sleep 0.01; done; awk 'BEGIN{{for(i=0;i<1000000;i++)printf \"x\"}}'",
        marker.display()
    );
    let execution = ShellProcessExecution::start_pipe(&shell(&script), &blob).unwrap();
    fs::rename(&blob, directory.0.join("blob-moved")).unwrap();
    fs::write(&blob, "not a directory").unwrap();
    fs::write(marker, "produce").unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), execution.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(first.kind(), io::ErrorKind::AlreadyExists);
    assert!(
        first.to_string().contains("blob"),
        "missing source blob context: {first}"
    );
    assert!(first.to_string().contains(blob.to_str().unwrap()));
    let cached = execution.try_result().unwrap_err();
    assert_eq!(cached.kind(), first.kind());
    assert_eq!(cached.to_string(), first.to_string());
    // Context is a custom io::Error, not a fabricated raw OS error at the outer
    // layer. Its actual original errno remains available through Error::source.
    for error in [&first, &cached] {
        assert_eq!(error.raw_os_error(), None);
        let mut source: &(dyn std::error::Error + 'static) = error;
        let mut errno = None;
        loop {
            if let Some(error) = source.downcast_ref::<io::Error>() {
                if error.raw_os_error().is_some() {
                    errno = error.raw_os_error();
                }
            }
            let Some(next) = source.source() else {
                break;
            };
            source = next;
        }
        assert_eq!(errno, Some(libc::EEXIST));
    }
    execution.settle().await.unwrap();
    execution.settle().await.unwrap();
    assert_eq!(
        execution.wait().await.unwrap_err().to_string(),
        first.to_string()
    );
    assert!(recover(&execution.state).drains.is_empty());
    assert!(recover(&execution.shared.native).is_none());
    reaped(execution.id());
}

#[tokio::test]
async fn poisoned_native_state_still_cancels_reaps_and_joins() {
    let directory = Directory::new();
    let execution =
        ShellProcessExecution::start_pipe(&shell("exec sleep 30"), &directory.0).unwrap();
    let shared = execution.shared.clone();
    let poisoned = thread::spawn(move || {
        let _guard = shared.native.lock().unwrap();
        panic!("intentional state poison");
    });
    assert!(poisoned.join().is_err());
    let error = tokio::time::timeout(Duration::from_secs(2), execution.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("poisoned"));
    assert!(execution.is_completed());
    execution.settle().await.unwrap();
    reaped(execution.id());
}

#[tokio::test]
async fn uncancelled_signal_exit_uses_source_unix_exit_code() {
    let directory = Directory::new();
    let execution =
        ShellProcessExecution::start_pipe(&shell("exec sleep 30"), &directory.0).unwrap();
    execution.send_signal(libc::SIGTERM).unwrap();
    assert_eq!(result(&execution).await.exit_code, 128 + libc::SIGTERM);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pty_interaction_raw_cursors_incremental_results_and_cleanup() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pty(
        &shell("stty -echo; printf 'A界😀'; read value; stty size; printf '%s' \"$value\""),
        helper(),
        &directory.0,
        24,
        80,
    )
    .await
    .unwrap();
    assert!(execution.is_pseudo_terminal());
    assert!(execution.stdout_path().is_none() && execution.stderr_path().is_none());
    until(|| execution.read_transcript(0).unwrap().0 == 4).await;
    assert_eq!(execution.read_transcript(3).unwrap(), (4, vec![0xde00]));
    execution.resize(40, 120).unwrap();
    execution.write_stdin("hello\n").await.unwrap();
    let actual = result(&execution).await;
    assert_eq!(actual.stdout, "A界😀40 120\r\nhello");
    assert!(actual.stderr.is_empty());
    assert_eq!(execution.read_result(0).unwrap().1, actual);
    let (_, suffix) = execution.read_result(4).unwrap();
    assert_eq!(suffix.stdout, "40 120\r\nhello");
    assert_eq!(
        execution.write_stdin("").await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    execution.settle().await.unwrap();
    no_temporary_files(&directory.0);
    reaped(execution.id());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pty_spill_and_incremental_suffix_preserve_borrowed_transcript_lifetime() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pty(
        &shell("awk 'BEGIN{for(i=0;i<70000;i++)printf \"o\"}'"),
        helper(),
        &directory.0,
        24,
        80,
    )
    .await
    .unwrap();
    let actual = result(&execution).await;
    let blob = actual.blob_path.clone().unwrap();
    assert!(
        fs::read_to_string(&blob)
            .unwrap()
            .ends_with(&"o".repeat(70000))
    );
    assert_eq!(
        execution.read_result(69000).unwrap().1.stdout,
        "o".repeat(1000)
    );
    assert_eq!(
        execution.read_result(0).unwrap().1.blob_path,
        Some(blob.clone())
    );
    drop(execution);
    no_temporary_files(&directory.0);
    assert!(blob.is_file());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn independent_transcript_cleanup_error_is_not_hidden_by_settlement() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pty(
        &shell("awk 'BEGIN{for(i=0;i<70000;i++)printf \"o\"}'"),
        helper(),
        &directory.0,
        24,
        80,
    )
    .await
    .unwrap();
    let completed = result(&execution).await;
    let (_, transcript) = execution
        .transcript
        .as_ref()
        .unwrap()
        .read_output(0)
        .unwrap();
    let path = transcript.temporary_path().unwrap().to_path_buf();
    fs::rename(&path, directory.0.join("retained-transcript")).unwrap();
    fs::create_dir(&path).unwrap();
    // This is a real cleanup remove_file failure on an output artifact, not a
    // replay of the process result or a filesystem authorization operation.
    let cleanup = execution.settle().await.unwrap_err();
    assert_eq!(cleanup.kind(), io::ErrorKind::IsADirectory);
    assert!(cleanup.to_string().contains("blob directory"));
    assert_eq!(execution.wait().await.unwrap(), completed);
    assert!(recover(&execution.state).drains.is_empty());
    assert!(recover(&execution.shared.native).is_none());
    reaped(execution.id());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropped_pty_writer_releases_gate_and_cancel_joins_terminal_drains() {
    let directory = Directory::new();
    let execution = ShellProcessExecution::start_pty(
        &shell("stty -echo -icanon; printf ready; exec sleep 30"),
        helper(),
        &directory.0,
        24,
        80,
    )
    .await
    .unwrap();
    until(|| execution.read_transcript(0).unwrap().0 >= 5).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            execution.write_stdin(&"x".repeat(4 << 20))
        )
        .await
        .is_err()
    );
    assert!(!execution.writing.load(Ordering::Acquire));
    execution.request_cancel();
    execution.settle().await.unwrap();
    assert!(recover(&execution.state).drains.is_empty());
    reaped(execution.id());
    no_temporary_files(&directory.0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pty_bridge_error_is_failure_and_startup_future_cancellation_reaps() {
    let directory = Directory::new();
    let helper_path = fixture("bridge-error-helper.sh");
    let execution =
        ShellProcessExecution::start_pty(&shell("exit 0"), &helper_path, &directory.0, 24, 80)
            .await
            .unwrap();
    assert!(
        execution
            .wait()
            .await
            .unwrap_err()
            .to_string()
            .contains("boom")
    );
    assert!(execution.is_completed());
    execution.settle().await.unwrap();
    let pid_path = directory.0.join("pid");
    let helper_path = fixture("stalled-helper.sh");
    let mut spec = shell("exit 0");
    spec.environment.insert(
        "PARROT_TEST_HELPER_PID_PATH".into(),
        Some(pid_path.clone().into_os_string()),
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            ShellProcessExecution::start_pty(&spec, &helper_path, &directory.0, 24, 80)
        )
        .await
        .is_err()
    );
    let pid: u32 = fs::read_to_string(pid_path).unwrap().parse().unwrap();
    reaped(pid);
}
